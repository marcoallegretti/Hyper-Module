#![doc = "Application-thread dispatch for compiled software control mappings."]
#![forbid(unsafe_code)]

use hyper_control_mappings::{
    ChannelParameter, ContinuousControlValue, CueParameter, Edge, GAIN_MILLIONTHS_MAX,
    MainParameter, PAN_MILLIONTHS_MAX, PAN_MILLIONTHS_MIN, SceneAction, SendParameter,
    TransportAction, TriggerBehavior,
};
use hyper_mapping_compiler::{
    CompiledControlBinding, CompiledControlMappingTable, CompiledControlScope,
    CompiledControlSource, CompiledControlTarget,
};

mod panel;

pub use panel::{
    PanelActionTarget, PanelAlreadyConnected, PanelBinding, PanelBindingRefusal,
    PanelBindingRefusalReason, PanelButtonBehavior, PanelControl, PanelDispatchError,
    PanelDispatchOutcome, PanelReleaseFailure,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlEvent {
    Cv { input_index: u8, value: i32 },
    Clock { edge: Edge },
    Reset { edge: Edge },
}

impl ControlEvent {
    const fn source(self) -> CompiledControlSource {
        match self {
            Self::Cv { input_index, .. } => CompiledControlSource::CvInput { input_index },
            Self::Clock { edge } => CompiledControlSource::ClockInput { edge },
            Self::Reset { edge } => CompiledControlSource::ResetInput { edge },
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ControlContext {
    mixer_page_active: bool,
    track_index: Option<usize>,
    scene_index: Option<usize>,
}

impl ControlContext {
    #[must_use]
    pub const fn new(
        mixer_page_active: bool,
        track_index: Option<usize>,
        scene_index: Option<usize>,
    ) -> Self {
        Self {
            mixer_page_active,
            track_index,
            scene_index,
        }
    }

    fn activates(self, scope: CompiledControlScope) -> bool {
        match scope {
            CompiledControlScope::Global => true,
            CompiledControlScope::MixerPage => self.mixer_page_active,
            CompiledControlScope::Track { track_index } => self.track_index == Some(track_index),
            CompiledControlScope::Scene { scene_index } => self.scene_index == Some(scene_index),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlBindingUnavailableReason {
    CueOutputUnavailable,
    SceneDisabled,
    SceneConsolidationRequired,
    SceneAudioNotPrepared,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlBindingAvailability {
    Live,
    Unavailable(ControlBindingUnavailableReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlBindingStatus {
    source_binding_index: usize,
    availability: ControlBindingAvailability,
}

impl ControlBindingStatus {
    #[must_use]
    pub const fn source_binding_index(self) -> usize {
        self.source_binding_index
    }

    #[must_use]
    pub const fn availability(self) -> ControlBindingAvailability {
        self.availability
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BooleanControlTarget {
    ChannelMute {
        channel_index: usize,
    },
    ChannelSolo {
        channel_index: usize,
    },
    SendEnabled {
        channel_index: usize,
        send_index: usize,
    },
    MainMute,
}

/// One continuous target a relative control moves rather than sets outright.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValueControlTarget {
    ChannelGain {
        channel_index: usize,
    },
    ChannelPan {
        channel_index: usize,
    },
    SendGain {
        channel_index: usize,
        send_index: usize,
    },
    SendPan {
        channel_index: usize,
        send_index: usize,
    },
    MainGain,
}

impl ValueControlTarget {
    const fn domain(self) -> (i32, i32) {
        match self {
            Self::ChannelGain { .. } | Self::SendGain { .. } | Self::MainGain => {
                (0, GAIN_MILLIONTHS_MAX)
            }
            Self::ChannelPan { .. } | Self::SendPan { .. } => {
                (PAN_MILLIONTHS_MIN, PAN_MILLIONTHS_MAX)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeControlTarget {
    Channel {
        channel_index: usize,
        parameter: ChannelParameter,
    },
    Send {
        channel_index: usize,
        send_index: usize,
        parameter: SendParameter,
    },
    Main {
        parameter: MainParameter,
    },
    Transport,
    Scene {
        scene_index: usize,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeControlCommand {
    ChannelGain {
        channel_index: usize,
        gain_millionths: u32,
    },
    ChannelPan {
        channel_index: usize,
        pan_millionths: i32,
    },
    ChannelMute {
        channel_index: usize,
        muted: bool,
    },
    ChannelSolo {
        channel_index: usize,
        solo: bool,
    },
    SendGain {
        channel_index: usize,
        send_index: usize,
        gain_millionths: u32,
    },
    SendPan {
        channel_index: usize,
        send_index: usize,
        pan_millionths: i32,
    },
    SendEnabled {
        channel_index: usize,
        send_index: usize,
        enabled: bool,
    },
    MainGain {
        gain_millionths: u32,
    },
    MainMute {
        muted: bool,
    },
    TransportStart,
    TransportStop,
    TransportLocateStartPreserve,
    SceneLaunch {
        scene_index: usize,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectControlStateUpdate {
    Unchanged,
    Boolean {
        target: BooleanControlTarget,
        value: bool,
    },
    Transport {
        rolling: bool,
    },
    Value {
        target: ValueControlTarget,
        value: i32,
    },
}

pub trait RuntimeControlSink {
    type Error;
    type DirectCommand;
    type DirectError;

    /// Validates one compiled target without changing runtime state.
    ///
    /// # Errors
    ///
    /// Returns a target-layout or capability rejection.
    fn validate_target(&self, target: RuntimeControlTarget) -> Result<(), Self::Error>;

    fn target_availability(&self, target: RuntimeControlTarget) -> ControlBindingAvailability {
        match target {
            RuntimeControlTarget::Scene { .. } => ControlBindingAvailability::Unavailable(
                ControlBindingUnavailableReason::SceneAudioNotPrepared,
            ),
            _ => ControlBindingAvailability::Live,
        }
    }
    fn boolean_state(&self, target: BooleanControlTarget) -> Option<bool>;
    fn transport_rolling(&self) -> Option<bool>;

    /// Reports one continuous target's present position in millionths.
    ///
    /// A relative control edits the target's own position rather than one it
    /// accumulated itself, so it has to be able to read that position back.
    fn value_state(&self, target: ValueControlTarget) -> Option<i32>;

    /// Sends one already mapped runtime command.
    ///
    /// # Errors
    ///
    /// Returns a target, saturation, or disconnection rejection without
    /// applying the command.
    fn try_send(&mut self, command: RuntimeControlCommand) -> Result<(), Self::Error>;

    /// Sends one direct software command and reports its shadow-state effect.
    ///
    /// # Errors
    ///
    /// Returns the direct command rejection without applying a shadow update.
    fn try_send_direct(
        &mut self,
        command: Self::DirectCommand,
    ) -> Result<DirectControlStateUpdate, Self::DirectError>;
}

#[derive(Debug)]
pub enum ControlRuntimeBuildErrorReason<E> {
    CapacityUnavailable,
    Target { binding_index: usize, source: E },
}

#[derive(Debug)]
pub struct ControlRuntimeBuildError<S: RuntimeControlSink> {
    reason: ControlRuntimeBuildErrorReason<S::Error>,
    table: CompiledControlMappingTable,
    sink: S,
}

impl<S: RuntimeControlSink> ControlRuntimeBuildError<S> {
    #[must_use]
    pub const fn binding_index(&self) -> Option<usize> {
        match &self.reason {
            ControlRuntimeBuildErrorReason::CapacityUnavailable => None,
            ControlRuntimeBuildErrorReason::Target { binding_index, .. } => Some(*binding_index),
        }
    }

    #[must_use]
    pub const fn reason(&self) -> &ControlRuntimeBuildErrorReason<S::Error> {
        &self.reason
    }

    #[must_use]
    pub fn into_authorities(self) -> (CompiledControlMappingTable, S) {
        (self.table, self.sink)
    }

    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        ControlRuntimeBuildErrorReason<S::Error>,
        CompiledControlMappingTable,
        S,
    ) {
        (self.reason, self.table, self.sink)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BooleanShadow {
    target: BooleanControlTarget,
    value: Option<bool>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ValueShadow {
    target: ValueControlTarget,
    value: Option<i32>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct MomentaryCapture(Option<u8>);

impl MomentaryCapture {
    fn binding(self) -> Option<usize> {
        self.0.map(usize::from)
    }

    fn replace(&mut self, runtime_index: Option<usize>) {
        self.0 = runtime_index.map(|index| {
            u8::try_from(index)
                .expect("a validated version-1 mapping table has at most 256 bindings")
        });
    }
}

#[derive(Debug)]
pub struct ControlRuntime<S: RuntimeControlSink> {
    table: CompiledControlMappingTable,
    statuses: Vec<ControlBindingStatus>,
    boolean_shadows: Vec<BooleanShadow>,
    value_shadows: Vec<ValueShadow>,
    transport_rolling: Option<bool>,
    clock_capture: MomentaryCapture,
    reset_capture: MomentaryCapture,
    panel: Option<panel::PanelBindings>,
    sink: S,
}

#[derive(Debug, Eq, PartialEq)]
pub enum ControlDispatchError<E> {
    Ambiguous {
        first_binding_index: usize,
        second_binding_index: usize,
        matching_binding_count: usize,
    },
    StateUnavailable {
        binding_index: usize,
    },
    Sink {
        binding_index: usize,
        command: RuntimeControlCommand,
        source: E,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlDispatchOutcome {
    NoMatch,
    Unavailable {
        binding_index: usize,
        reason: ControlBindingUnavailableReason,
    },
    Sent {
        binding_index: usize,
        command: RuntimeControlCommand,
    },
}

impl<S: RuntimeControlSink> ControlRuntime<S> {
    /// Binds every executable target before publishing the runtime.
    ///
    /// # Errors
    ///
    /// Returns both input authorities when status allocation or live-target
    /// validation fails. Unsupported bindings remain valid status entries and
    /// do not reach the sink.
    pub fn try_new(
        table: CompiledControlMappingTable,
        sink: S,
    ) -> Result<Self, ControlRuntimeBuildError<S>> {
        match Self::try_new_inner(table, sink) {
            Ok(runtime) => Ok(runtime),
            Err(error) => Err(error),
        }
    }

    fn try_new_inner(
        table: CompiledControlMappingTable,
        sink: S,
    ) -> Result<Self, ControlRuntimeBuildError<S>> {
        let mut statuses = Vec::new();
        if statuses.try_reserve_exact(table.bindings().len()).is_err() {
            return Err(ControlRuntimeBuildError {
                reason: ControlRuntimeBuildErrorReason::CapacityUnavailable,
                table,
                sink,
            });
        }
        let mut boolean_shadows = Vec::new();
        if boolean_shadows
            .try_reserve_exact(table.bindings().len())
            .is_err()
        {
            return Err(ControlRuntimeBuildError {
                reason: ControlRuntimeBuildErrorReason::CapacityUnavailable,
                table,
                sink,
            });
        }

        for binding in table.bindings() {
            let mut availability = binding_availability(*binding);
            if availability == ControlBindingAvailability::Live
                && let Some(target) = runtime_target(*binding)
            {
                availability = sink.target_availability(target);
            }
            if availability == ControlBindingAvailability::Live {
                let target = runtime_target(*binding)
                    .expect("a live compiled binding has one executable runtime target");
                if let Err(source) = sink.validate_target(target) {
                    return Err(ControlRuntimeBuildError {
                        reason: ControlRuntimeBuildErrorReason::Target {
                            binding_index: binding.source_binding_index(),
                            source,
                        },
                        table,
                        sink,
                    });
                }
                if binding.trigger_behavior() == Some(TriggerBehavior::Toggle)
                    && let Some(target) = boolean_target(*binding)
                    && !boolean_shadows
                        .iter()
                        .any(|shadow: &BooleanShadow| shadow.target == target)
                {
                    boolean_shadows.push(BooleanShadow {
                        target,
                        value: sink.boolean_state(target),
                    });
                }
            }
            statuses.push(ControlBindingStatus {
                source_binding_index: binding.source_binding_index(),
                availability,
            });
        }
        let transport_rolling = sink.transport_rolling();
        Ok(Self {
            table,
            statuses,
            boolean_shadows,
            value_shadows: Vec::new(),
            transport_rolling,
            clock_capture: MomentaryCapture::default(),
            reset_capture: MomentaryCapture::default(),
            panel: None,
            sink,
        })
    }

    #[must_use]
    pub fn statuses(&self) -> &[ControlBindingStatus] {
        &self.statuses
    }

    #[must_use]
    pub const fn sink(&self) -> &S {
        &self.sink
    }

    /// Returns exclusive access to the sole runtime sink owner.
    #[must_use]
    pub const fn sink_mut(&mut self) -> &mut S {
        &mut self.sink
    }

    /// Dispatches at most one active binding and never allocates.
    ///
    /// With no selected precedence between simultaneously active scopes, two
    /// matching activations are rejected before any command is sent. A
    /// successfully delivered momentary activation captures its physical edge
    /// source until release delivery, so later context changes cannot strand
    /// the target or transfer the paired edge to another binding.
    ///
    /// # Errors
    ///
    /// Returns an ambiguity, missing toggle-state authority, or the sink's
    /// original command rejection.
    pub fn dispatch(
        &mut self,
        event: ControlEvent,
        context: ControlContext,
    ) -> Result<ControlDispatchOutcome, ControlDispatchError<S::Error>> {
        let source = event.source();
        let captured = self.captured_binding(source);
        let mut selected = captured;
        let mut second = None;
        let mut count = usize::from(captured.is_some());
        if captured.is_none() {
            for (runtime_index, binding) in self.table.bindings().iter().enumerate() {
                if binding.source() == source && context.activates(binding.scope()) {
                    count += 1;
                    if selected.is_none() {
                        selected = Some(runtime_index);
                    } else if second.is_none() {
                        second = Some(runtime_index);
                    }
                }
            }
        }
        if count > 1
            && let (Some(first), Some(second)) = (selected, second)
        {
            return Err(ControlDispatchError::Ambiguous {
                first_binding_index: self.table.bindings()[first].source_binding_index(),
                second_binding_index: self.table.bindings()[second].source_binding_index(),
                matching_binding_count: count,
            });
        }
        let Some(runtime_index) = selected else {
            return Ok(ControlDispatchOutcome::NoMatch);
        };
        let binding = self.table.bindings()[runtime_index];
        let binding_index = binding.source_binding_index();
        if let ControlBindingAvailability::Unavailable(reason) =
            self.statuses[runtime_index].availability
        {
            return Ok(ControlDispatchOutcome::Unavailable {
                binding_index,
                reason,
            });
        }
        self.refresh_missing_state();
        let command = self
            .command(binding, event)
            .ok_or(ControlDispatchError::StateUnavailable { binding_index })?;
        self.sink
            .try_send(command)
            .map_err(|source| ControlDispatchError::Sink {
                binding_index,
                command,
                source,
            })?;
        self.commit_momentary_cycle(runtime_index, event);
        self.commit_shadow(command);
        Ok(ControlDispatchOutcome::Sent {
            binding_index,
            command,
        })
    }

    /// Sends one direct software command through the same queue owner as mapped
    /// events and commits its state effect only after successful enqueue.
    ///
    /// # Errors
    ///
    /// Returns the sink's direct command rejection without changing shadows.
    pub fn try_send_direct(&mut self, command: S::DirectCommand) -> Result<(), S::DirectError> {
        let update = self.sink.try_send_direct(command)?;
        self.commit_direct_update(update);
        Ok(())
    }

    fn refresh_missing_state(&mut self) {
        for shadow in &mut self.boolean_shadows {
            if shadow.value.is_none() {
                shadow.value = self.sink.boolean_state(shadow.target);
            }
        }
        for shadow in &mut self.value_shadows {
            if shadow.value.is_none() {
                shadow.value = self.sink.value_state(shadow.target);
            }
        }
        if self.transport_rolling.is_none() {
            self.transport_rolling = self.sink.transport_rolling();
        }
    }

    fn command(
        &self,
        binding: CompiledControlBinding,
        event: ControlEvent,
    ) -> Option<RuntimeControlCommand> {
        match event {
            ControlEvent::Cv { value, .. } => continuous_command(binding, value),
            ControlEvent::Clock { .. } | ControlEvent::Reset { .. } => trigger_command(
                binding,
                event,
                &self.boolean_shadows,
                self.transport_rolling,
            ),
        }
    }

    fn commit_shadow(&mut self, command: RuntimeControlCommand) {
        if let Some((target, value)) = command_boolean_state(command)
            && let Some(shadow) = self
                .boolean_shadows
                .iter_mut()
                .find(|shadow| shadow.target == target)
        {
            shadow.value = Some(value);
        }
        if let Some((target, value)) = command_value_state(command) {
            self.commit_value_shadow(target, value);
        }
        match command {
            RuntimeControlCommand::TransportStart => self.transport_rolling = Some(true),
            RuntimeControlCommand::TransportStop => self.transport_rolling = Some(false),
            RuntimeControlCommand::TransportLocateStartPreserve
            | RuntimeControlCommand::SceneLaunch { .. }
            | RuntimeControlCommand::ChannelGain { .. }
            | RuntimeControlCommand::ChannelPan { .. }
            | RuntimeControlCommand::ChannelMute { .. }
            | RuntimeControlCommand::ChannelSolo { .. }
            | RuntimeControlCommand::SendGain { .. }
            | RuntimeControlCommand::SendPan { .. }
            | RuntimeControlCommand::SendEnabled { .. }
            | RuntimeControlCommand::MainGain { .. }
            | RuntimeControlCommand::MainMute { .. } => {}
        }
    }

    fn commit_direct_update(&mut self, update: DirectControlStateUpdate) {
        match update {
            DirectControlStateUpdate::Unchanged => {}
            DirectControlStateUpdate::Boolean { target, value } => {
                if let Some(shadow) = self
                    .boolean_shadows
                    .iter_mut()
                    .find(|shadow| shadow.target == target)
                {
                    shadow.value = Some(value);
                }
            }
            DirectControlStateUpdate::Transport { rolling } => {
                self.transport_rolling = Some(rolling);
            }
            DirectControlStateUpdate::Value { target, value } => {
                self.commit_value_shadow(target, value);
            }
        }
    }

    fn commit_value_shadow(&mut self, target: ValueControlTarget, value: i32) {
        if let Some(shadow) = self
            .value_shadows
            .iter_mut()
            .find(|shadow| shadow.target == target)
        {
            shadow.value = Some(value);
        }
    }

    fn captured_binding(&self, source: CompiledControlSource) -> Option<usize> {
        match source {
            CompiledControlSource::ClockInput { .. } => self.clock_capture.binding(),
            CompiledControlSource::ResetInput { .. } => self.reset_capture.binding(),
            CompiledControlSource::CvInput { .. } => None,
        }
    }

    fn commit_momentary_cycle(&mut self, runtime_index: usize, event: ControlEvent) {
        let binding = self.table.bindings()[runtime_index];
        if binding.trigger_behavior() != Some(TriggerBehavior::Momentary) {
            return;
        }
        let capture = (binding.source() == event.source()).then_some(runtime_index);
        match event {
            ControlEvent::Clock { .. } => self.clock_capture.replace(capture),
            ControlEvent::Reset { .. } => self.reset_capture.replace(capture),
            ControlEvent::Cv { .. } => {}
        }
    }
}

fn binding_availability(binding: CompiledControlBinding) -> ControlBindingAvailability {
    match binding.target() {
        CompiledControlTarget::MixerCue { .. } => ControlBindingAvailability::Unavailable(
            ControlBindingUnavailableReason::CueOutputUnavailable,
        ),
        CompiledControlTarget::Scene { .. }
        | CompiledControlTarget::MixerChannel { .. }
        | CompiledControlTarget::MixerSend { .. }
        | CompiledControlTarget::MixerMain { .. }
        | CompiledControlTarget::Transport { .. } => ControlBindingAvailability::Live,
    }
}

fn runtime_target(binding: CompiledControlBinding) -> Option<RuntimeControlTarget> {
    match binding.target() {
        CompiledControlTarget::MixerChannel {
            channel_index,
            parameter,
        } => Some(RuntimeControlTarget::Channel {
            channel_index,
            parameter,
        }),
        CompiledControlTarget::MixerSend { address, parameter } => {
            Some(RuntimeControlTarget::Send {
                channel_index: address.channel_index(),
                send_index: address.send_index(),
                parameter,
            })
        }
        CompiledControlTarget::MixerMain { parameter } => {
            Some(RuntimeControlTarget::Main { parameter })
        }
        CompiledControlTarget::Transport { .. } => Some(RuntimeControlTarget::Transport),
        CompiledControlTarget::Scene { scene_index, .. } => {
            Some(RuntimeControlTarget::Scene { scene_index })
        }
        CompiledControlTarget::MixerCue {
            parameter: CueParameter::Gain | CueParameter::Mute,
        } => None,
    }
}

fn boolean_target(binding: CompiledControlBinding) -> Option<BooleanControlTarget> {
    match binding.target() {
        CompiledControlTarget::MixerChannel {
            channel_index,
            parameter: ChannelParameter::Mute,
        } => Some(BooleanControlTarget::ChannelMute { channel_index }),
        CompiledControlTarget::MixerChannel {
            channel_index,
            parameter: ChannelParameter::Solo,
        } => Some(BooleanControlTarget::ChannelSolo { channel_index }),
        CompiledControlTarget::MixerSend {
            address,
            parameter: SendParameter::Enabled,
        } => Some(BooleanControlTarget::SendEnabled {
            channel_index: address.channel_index(),
            send_index: address.send_index(),
        }),
        CompiledControlTarget::MixerMain {
            parameter: MainParameter::Mute,
        } => Some(BooleanControlTarget::MainMute),
        CompiledControlTarget::MixerChannel { .. }
        | CompiledControlTarget::MixerSend { .. }
        | CompiledControlTarget::MixerMain { .. }
        | CompiledControlTarget::MixerCue { .. }
        | CompiledControlTarget::Scene { .. }
        | CompiledControlTarget::Transport { .. } => None,
    }
}

fn continuous_command(
    binding: CompiledControlBinding,
    input: i32,
) -> Option<RuntimeControlCommand> {
    let value = binding.map_continuous(input)?;
    match (binding.target(), value) {
        (
            CompiledControlTarget::MixerChannel { channel_index, .. },
            ContinuousControlValue::GainMillionths(gain_millionths),
        ) => Some(RuntimeControlCommand::ChannelGain {
            channel_index,
            gain_millionths,
        }),
        (
            CompiledControlTarget::MixerChannel { channel_index, .. },
            ContinuousControlValue::PanMillionths(pan_millionths),
        ) => Some(RuntimeControlCommand::ChannelPan {
            channel_index,
            pan_millionths,
        }),
        (
            CompiledControlTarget::MixerSend { address, .. },
            ContinuousControlValue::GainMillionths(gain_millionths),
        ) => Some(RuntimeControlCommand::SendGain {
            channel_index: address.channel_index(),
            send_index: address.send_index(),
            gain_millionths,
        }),
        (
            CompiledControlTarget::MixerSend { address, .. },
            ContinuousControlValue::PanMillionths(pan_millionths),
        ) => Some(RuntimeControlCommand::SendPan {
            channel_index: address.channel_index(),
            send_index: address.send_index(),
            pan_millionths,
        }),
        (
            CompiledControlTarget::MixerMain { .. },
            ContinuousControlValue::GainMillionths(gain_millionths),
        ) => Some(RuntimeControlCommand::MainGain { gain_millionths }),
        (CompiledControlTarget::MixerMain { .. }, ContinuousControlValue::PanMillionths(_))
        | (
            CompiledControlTarget::MixerCue { .. }
            | CompiledControlTarget::Scene { .. }
            | CompiledControlTarget::Transport { .. },
            _,
        ) => None,
    }
}

fn trigger_command(
    binding: CompiledControlBinding,
    event: ControlEvent,
    shadows: &[BooleanShadow],
    transport_rolling: Option<bool>,
) -> Option<RuntimeControlCommand> {
    match binding.target() {
        CompiledControlTarget::MixerChannel {
            channel_index,
            parameter: ChannelParameter::Mute,
        } => Some(RuntimeControlCommand::ChannelMute {
            channel_index,
            muted: trigger_boolean_value(
                binding,
                event,
                shadows,
                BooleanControlTarget::ChannelMute { channel_index },
            )?,
        }),
        CompiledControlTarget::MixerChannel {
            channel_index,
            parameter: ChannelParameter::Solo,
        } => Some(RuntimeControlCommand::ChannelSolo {
            channel_index,
            solo: trigger_boolean_value(
                binding,
                event,
                shadows,
                BooleanControlTarget::ChannelSolo { channel_index },
            )?,
        }),
        CompiledControlTarget::MixerSend {
            address,
            parameter: SendParameter::Enabled,
        } => Some(RuntimeControlCommand::SendEnabled {
            channel_index: address.channel_index(),
            send_index: address.send_index(),
            enabled: trigger_boolean_value(
                binding,
                event,
                shadows,
                BooleanControlTarget::SendEnabled {
                    channel_index: address.channel_index(),
                    send_index: address.send_index(),
                },
            )?,
        }),
        CompiledControlTarget::MixerMain {
            parameter: MainParameter::Mute,
        } => Some(RuntimeControlCommand::MainMute {
            muted: trigger_boolean_value(binding, event, shadows, BooleanControlTarget::MainMute)?,
        }),
        CompiledControlTarget::Transport { action } => match action {
            TransportAction::Play => Some(RuntimeControlCommand::TransportStart),
            TransportAction::Stop => Some(RuntimeControlCommand::TransportStop),
            TransportAction::Toggle => Some(if transport_rolling? {
                RuntimeControlCommand::TransportStop
            } else {
                RuntimeControlCommand::TransportStart
            }),
            TransportAction::Reset => Some(RuntimeControlCommand::TransportLocateStartPreserve),
        },
        CompiledControlTarget::Scene {
            scene_index,
            action: SceneAction::Launch,
        } => Some(RuntimeControlCommand::SceneLaunch { scene_index }),
        CompiledControlTarget::MixerChannel { .. }
        | CompiledControlTarget::MixerSend { .. }
        | CompiledControlTarget::MixerMain { .. }
        | CompiledControlTarget::MixerCue { .. } => None,
    }
}

fn trigger_boolean_value(
    binding: CompiledControlBinding,
    event: ControlEvent,
    shadows: &[BooleanShadow],
    target: BooleanControlTarget,
) -> Option<bool> {
    match binding.trigger_behavior()? {
        TriggerBehavior::Toggle => Some(!shadow_value(shadows, target)?),
        TriggerBehavior::Momentary => Some(binding.source() == event.source()),
        TriggerBehavior::Invoke => None,
    }
}

fn shadow_value(shadows: &[BooleanShadow], target: BooleanControlTarget) -> Option<bool> {
    shadows
        .iter()
        .find(|shadow| shadow.target == target)
        .and_then(|shadow| shadow.value)
}

fn command_boolean_state(command: RuntimeControlCommand) -> Option<(BooleanControlTarget, bool)> {
    match command {
        RuntimeControlCommand::ChannelMute {
            channel_index,
            muted,
        } => Some((BooleanControlTarget::ChannelMute { channel_index }, muted)),
        RuntimeControlCommand::ChannelSolo {
            channel_index,
            solo,
        } => Some((BooleanControlTarget::ChannelSolo { channel_index }, solo)),
        RuntimeControlCommand::SendEnabled {
            channel_index,
            send_index,
            enabled,
        } => Some((
            BooleanControlTarget::SendEnabled {
                channel_index,
                send_index,
            },
            enabled,
        )),
        RuntimeControlCommand::MainMute { muted } => Some((BooleanControlTarget::MainMute, muted)),
        RuntimeControlCommand::ChannelGain { .. }
        | RuntimeControlCommand::ChannelPan { .. }
        | RuntimeControlCommand::SendGain { .. }
        | RuntimeControlCommand::SendPan { .. }
        | RuntimeControlCommand::MainGain { .. }
        | RuntimeControlCommand::TransportStart
        | RuntimeControlCommand::TransportStop
        | RuntimeControlCommand::TransportLocateStartPreserve
        | RuntimeControlCommand::SceneLaunch { .. } => None,
    }
}

fn command_value_state(command: RuntimeControlCommand) -> Option<(ValueControlTarget, i32)> {
    match command {
        RuntimeControlCommand::ChannelGain {
            channel_index,
            gain_millionths,
        } => Some((
            ValueControlTarget::ChannelGain { channel_index },
            i32::try_from(gain_millionths).ok()?,
        )),
        RuntimeControlCommand::ChannelPan {
            channel_index,
            pan_millionths,
        } => Some((
            ValueControlTarget::ChannelPan { channel_index },
            pan_millionths,
        )),
        RuntimeControlCommand::SendGain {
            channel_index,
            send_index,
            gain_millionths,
        } => Some((
            ValueControlTarget::SendGain {
                channel_index,
                send_index,
            },
            i32::try_from(gain_millionths).ok()?,
        )),
        RuntimeControlCommand::SendPan {
            channel_index,
            send_index,
            pan_millionths,
        } => Some((
            ValueControlTarget::SendPan {
                channel_index,
                send_index,
            },
            pan_millionths,
        )),
        RuntimeControlCommand::MainGain { gain_millionths } => Some((
            ValueControlTarget::MainGain,
            i32::try_from(gain_millionths).ok()?,
        )),
        RuntimeControlCommand::ChannelMute { .. }
        | RuntimeControlCommand::ChannelSolo { .. }
        | RuntimeControlCommand::SendEnabled { .. }
        | RuntimeControlCommand::MainMute { .. }
        | RuntimeControlCommand::TransportStart
        | RuntimeControlCommand::TransportStop
        | RuntimeControlCommand::TransportLocateStartPreserve
        | RuntimeControlCommand::SceneLaunch { .. } => None,
    }
}
