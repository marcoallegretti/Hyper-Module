use core::error::Error;
use core::fmt;

use hyper_audio_engine::{LinearGain, Pan, ParameterError};
use hyper_control_mappings::{ChannelParameter, MainParameter, SendParameter};
use hyper_control_runtime::{
    BooleanControlTarget, ControlBindingAvailability, ControlBindingUnavailableReason,
    ControlRuntime, DirectControlStateUpdate, RuntimeControlCommand, RuntimeControlSink,
    RuntimeControlTarget, ValueControlTarget, ValueDomain,
};
use hyper_mixer_runtime::{
    LocateState, MixerChannelHandle, MixerParameterCommand, MixerParameterCommandSendError,
    MixerParameterCommandSender, MixerParameterResolveError, MixerParameterStateSnapshot,
    MixerParameterTargetError, MixerSendHandle, PlaybackRecoveryCheckpoint,
    PlaybackRecoveryTransportState, TransportCommand, TransportCommandSendError,
    TransportCommandSender,
};

pub type DesktopControlMappingRuntime = ControlRuntime<DesktopControlSink>;

/// The widest gain, in millionths, [`LinearGain::from_millionths`] accepts.
const GAIN_MILLIONTHS_MAX: i32 = 2_000_000;
/// The pan extremes, in millionths, [`Pan::from_millionths`] accepts.
const PAN_MILLIONTHS_MAX: i32 = 1_000_000;

fn gain_millionths(gain: LinearGain) -> i32 {
    i32::try_from(gain.to_millionths())
        .expect("a linear gain is bounded at 2.0, so its millionths fit a signed value")
}

fn value_from_state(
    state: &MixerParameterStateSnapshot,
    target: ValueControlTarget,
) -> Option<i32> {
    match target {
        ValueControlTarget::ChannelGain { channel_index } => Some(gain_millionths(
            state.channel(channel_index)?.parameters().gain,
        )),
        ValueControlTarget::ChannelPan { channel_index } => Some(
            state
                .channel(channel_index)?
                .parameters()
                .pan
                .to_millionths(),
        ),
        ValueControlTarget::SendGain {
            channel_index,
            send_index,
        } => Some(gain_millionths(
            state
                .channel(channel_index)?
                .send(send_index)?
                .parameters()
                .gain,
        )),
        ValueControlTarget::SendPan {
            channel_index,
            send_index,
        } => Some(
            state
                .channel(channel_index)?
                .send(send_index)?
                .parameters()
                .pan
                .to_millionths(),
        ),
        ValueControlTarget::MainGain => Some(gain_millionths(state.main_gain())),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DesktopDirectControlCommand {
    Mixer(MixerParameterCommand),
    Transport(TransportCommand),
}

#[derive(Debug, PartialEq)]
pub enum DesktopDirectControlError {
    Mixer(MixerParameterCommandSendError),
    Transport(TransportCommandSendError),
}

impl fmt::Display for DesktopDirectControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mixer(source) => source.fmt(formatter),
            Self::Transport(source) => source.fmt(formatter),
        }
    }
}

impl Error for DesktopDirectControlError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Mixer(source) => Some(source),
            Self::Transport(source) => Some(source),
        }
    }
}

#[derive(Debug)]
pub enum DesktopControlSinkError {
    CommandDomain,
    Parameter(ParameterError),
    Resolve(MixerParameterResolveError),
    Target(MixerParameterTargetError),
    MixerSend(MixerParameterCommandSendError),
    TransportSend(TransportCommandSendError),
}

impl fmt::Display for DesktopControlSinkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CommandDomain => formatter.write_str("command belongs to the transport domain"),
            Self::Parameter(source) => write!(formatter, "mapped parameter is invalid: {source}"),
            Self::Resolve(source) => write!(formatter, "mapped target is unresolved: {source}"),
            Self::Target(source) => write!(formatter, "mapped target is invalid: {source}"),
            Self::MixerSend(source) => write!(formatter, "mixer command failed: {source}"),
            Self::TransportSend(source) => write!(formatter, "transport command failed: {source}"),
        }
    }
}

impl Error for DesktopControlSinkError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CommandDomain => None,
            Self::Parameter(source) => Some(source),
            Self::Resolve(source) => Some(source),
            Self::Target(source) => Some(source),
            Self::MixerSend(source) => Some(source),
            Self::TransportSend(source) => Some(source),
        }
    }
}

#[derive(Debug)]
pub struct DesktopControlSink {
    transport: TransportCommandSender,
    mixer: MixerParameterCommandSender,
    recovery: PlaybackRecoveryCheckpoint,
}

impl DesktopControlSink {
    #[must_use]
    pub const fn new(
        transport: TransportCommandSender,
        mixer: MixerParameterCommandSender,
        recovery: PlaybackRecoveryCheckpoint,
    ) -> Self {
        Self {
            transport,
            mixer,
            recovery,
        }
    }

    /// Resolves one persisted track identity through the owned mixer layout.
    ///
    /// # Errors
    ///
    /// Returns a missing-channel rejection.
    pub fn resolve_channel(
        &self,
        track_id: &str,
    ) -> Result<MixerChannelHandle, MixerParameterResolveError> {
        self.mixer.resolve_channel(track_id)
    }

    /// Resolves one persisted send identity through the owned mixer layout.
    ///
    /// # Errors
    ///
    /// Returns a missing-send rejection.
    pub fn resolve_send(
        &self,
        send_id: &str,
    ) -> Result<MixerSendHandle, MixerParameterResolveError> {
        self.mixer.resolve_send(send_id)
    }

    pub(crate) const fn transport_sender(&self) -> &TransportCommandSender {
        &self.transport
    }

    pub(crate) const fn mixer_sender(&self) -> &MixerParameterCommandSender {
        &self.mixer
    }

    pub(crate) fn try_send_mixer_fence(
        &mut self,
    ) -> Result<Option<u64>, DesktopDirectControlError> {
        self.mixer
            .try_send_fence()
            .map_err(DesktopDirectControlError::Mixer)
    }

    fn direct_mixer_update(
        &self,
        command: MixerParameterCommand,
    ) -> Result<DirectControlStateUpdate, MixerParameterTargetError> {
        match command {
            MixerParameterCommand::ChannelMute { channel, muted } => {
                Ok(DirectControlStateUpdate::Boolean {
                    target: BooleanControlTarget::ChannelMute {
                        channel_index: self.mixer.channel_index(channel)?,
                    },
                    value: muted,
                })
            }
            MixerParameterCommand::ChannelSolo { channel, solo } => {
                Ok(DirectControlStateUpdate::Boolean {
                    target: BooleanControlTarget::ChannelSolo {
                        channel_index: self.mixer.channel_index(channel)?,
                    },
                    value: solo,
                })
            }
            MixerParameterCommand::SendEnabled { send, enabled } => {
                let (channel_index, send_index) = self.mixer.send_address(send)?;
                Ok(DirectControlStateUpdate::Boolean {
                    target: BooleanControlTarget::SendEnabled {
                        channel_index,
                        send_index,
                    },
                    value: enabled,
                })
            }
            MixerParameterCommand::MainMute { muted } => Ok(DirectControlStateUpdate::Boolean {
                target: BooleanControlTarget::MainMute,
                value: muted,
            }),
            MixerParameterCommand::ChannelGain { channel, gain } => {
                Ok(DirectControlStateUpdate::Value {
                    target: ValueControlTarget::ChannelGain {
                        channel_index: self.mixer.channel_index(channel)?,
                    },
                    value: gain_millionths(gain),
                })
            }
            MixerParameterCommand::ChannelPan { channel, pan } => {
                Ok(DirectControlStateUpdate::Value {
                    target: ValueControlTarget::ChannelPan {
                        channel_index: self.mixer.channel_index(channel)?,
                    },
                    value: pan.to_millionths(),
                })
            }
            MixerParameterCommand::SendGain { send, gain } => {
                let (channel_index, send_index) = self.mixer.send_address(send)?;
                Ok(DirectControlStateUpdate::Value {
                    target: ValueControlTarget::SendGain {
                        channel_index,
                        send_index,
                    },
                    value: gain_millionths(gain),
                })
            }
            MixerParameterCommand::SendPan { send, pan } => {
                let (channel_index, send_index) = self.mixer.send_address(send)?;
                Ok(DirectControlStateUpdate::Value {
                    target: ValueControlTarget::SendPan {
                        channel_index,
                        send_index,
                    },
                    value: pan.to_millionths(),
                })
            }
            MixerParameterCommand::MainGain { gain } => Ok(DirectControlStateUpdate::Value {
                target: ValueControlTarget::MainGain,
                value: gain_millionths(gain),
            }),
            MixerParameterCommand::Fence(_) => Ok(DirectControlStateUpdate::Unchanged),
        }
    }

    fn mixer_command(
        &self,
        command: RuntimeControlCommand,
    ) -> Result<MixerParameterCommand, DesktopControlSinkError> {
        match command {
            RuntimeControlCommand::ChannelGain {
                channel_index,
                gain_millionths,
            } => Ok(MixerParameterCommand::ChannelGain {
                channel: self
                    .mixer
                    .resolve_channel_index(channel_index)
                    .map_err(DesktopControlSinkError::Resolve)?,
                gain: LinearGain::from_millionths(gain_millionths)
                    .map_err(DesktopControlSinkError::Parameter)?,
            }),
            RuntimeControlCommand::ChannelPan {
                channel_index,
                pan_millionths,
            } => Ok(MixerParameterCommand::ChannelPan {
                channel: self
                    .mixer
                    .resolve_channel_index(channel_index)
                    .map_err(DesktopControlSinkError::Resolve)?,
                pan: Pan::from_millionths(pan_millionths)
                    .map_err(DesktopControlSinkError::Parameter)?,
            }),
            RuntimeControlCommand::ChannelMute {
                channel_index,
                muted,
            } => Ok(MixerParameterCommand::ChannelMute {
                channel: self
                    .mixer
                    .resolve_channel_index(channel_index)
                    .map_err(DesktopControlSinkError::Resolve)?,
                muted,
            }),
            RuntimeControlCommand::ChannelSolo {
                channel_index,
                solo,
            } => Ok(MixerParameterCommand::ChannelSolo {
                channel: self
                    .mixer
                    .resolve_channel_index(channel_index)
                    .map_err(DesktopControlSinkError::Resolve)?,
                solo,
            }),
            RuntimeControlCommand::SendGain {
                channel_index,
                send_index,
                gain_millionths,
            } => Ok(MixerParameterCommand::SendGain {
                send: self
                    .mixer
                    .resolve_send_address(channel_index, send_index)
                    .map_err(DesktopControlSinkError::Resolve)?,
                gain: LinearGain::from_millionths(gain_millionths)
                    .map_err(DesktopControlSinkError::Parameter)?,
            }),
            RuntimeControlCommand::SendPan {
                channel_index,
                send_index,
                pan_millionths,
            } => Ok(MixerParameterCommand::SendPan {
                send: self
                    .mixer
                    .resolve_send_address(channel_index, send_index)
                    .map_err(DesktopControlSinkError::Resolve)?,
                pan: Pan::from_millionths(pan_millionths)
                    .map_err(DesktopControlSinkError::Parameter)?,
            }),
            RuntimeControlCommand::SendEnabled {
                channel_index,
                send_index,
                enabled,
            } => Ok(MixerParameterCommand::SendEnabled {
                send: self
                    .mixer
                    .resolve_send_address(channel_index, send_index)
                    .map_err(DesktopControlSinkError::Resolve)?,
                enabled,
            }),
            RuntimeControlCommand::MainGain { gain_millionths } => {
                Ok(MixerParameterCommand::MainGain {
                    gain: LinearGain::from_millionths(gain_millionths)
                        .map_err(DesktopControlSinkError::Parameter)?,
                })
            }
            RuntimeControlCommand::MainMute { muted } => {
                Ok(MixerParameterCommand::MainMute { muted })
            }
            RuntimeControlCommand::TransportStart
            | RuntimeControlCommand::TransportStop
            | RuntimeControlCommand::TransportLocateStartPreserve
            | RuntimeControlCommand::SceneLaunch { .. } => {
                Err(DesktopControlSinkError::CommandDomain)
            }
        }
    }
}

impl RuntimeControlSink for DesktopControlSink {
    type Error = DesktopControlSinkError;
    type DirectCommand = DesktopDirectControlCommand;
    type DirectError = DesktopDirectControlError;

    fn validate_target(&self, target: RuntimeControlTarget) -> Result<(), Self::Error> {
        let command = match target {
            RuntimeControlTarget::Channel {
                channel_index,
                parameter,
            } => match parameter {
                ChannelParameter::Gain => RuntimeControlCommand::ChannelGain {
                    channel_index,
                    gain_millionths: 1_000_000,
                },
                ChannelParameter::Pan => RuntimeControlCommand::ChannelPan {
                    channel_index,
                    pan_millionths: 0,
                },
                ChannelParameter::Mute => RuntimeControlCommand::ChannelMute {
                    channel_index,
                    muted: false,
                },
                ChannelParameter::Solo => RuntimeControlCommand::ChannelSolo {
                    channel_index,
                    solo: false,
                },
            },
            RuntimeControlTarget::Send {
                channel_index,
                send_index,
                parameter,
            } => match parameter {
                SendParameter::Gain => RuntimeControlCommand::SendGain {
                    channel_index,
                    send_index,
                    gain_millionths: 1_000_000,
                },
                SendParameter::Pan => RuntimeControlCommand::SendPan {
                    channel_index,
                    send_index,
                    pan_millionths: 0,
                },
                SendParameter::Enabled => RuntimeControlCommand::SendEnabled {
                    channel_index,
                    send_index,
                    enabled: false,
                },
            },
            RuntimeControlTarget::Main { parameter } => match parameter {
                MainParameter::Gain => RuntimeControlCommand::MainGain {
                    gain_millionths: 1_000_000,
                },
                MainParameter::Mute => RuntimeControlCommand::MainMute { muted: false },
            },
            RuntimeControlTarget::Transport => return Ok(()),
            RuntimeControlTarget::Scene { scene_index } => {
                return self
                    .transport
                    .validate_command(TransportCommand::LaunchScene { scene_index })
                    .map_err(DesktopControlSinkError::TransportSend);
            }
        };
        let command = self.mixer_command(command)?;
        self.mixer
            .validate_command(command)
            .map_err(DesktopControlSinkError::Target)
    }

    fn target_availability(&self, target: RuntimeControlTarget) -> ControlBindingAvailability {
        let RuntimeControlTarget::Scene { scene_index } = target else {
            return ControlBindingAvailability::Live;
        };
        match self
            .transport
            .validate_command(TransportCommand::LaunchScene { scene_index })
        {
            Ok(()) => ControlBindingAvailability::Live,
            Err(TransportCommandSendError::SceneUnavailable { reason, .. }) => {
                let reason = match reason {
                    hyper_media_cache::PreparedSceneAvailability::Disabled => {
                        ControlBindingUnavailableReason::SceneDisabled
                    }
                    hyper_media_cache::PreparedSceneAvailability::ConsolidationRequired => {
                        ControlBindingUnavailableReason::SceneConsolidationRequired
                    }
                    hyper_media_cache::PreparedSceneAvailability::NotPrepared
                    | hyper_media_cache::PreparedSceneAvailability::Ready => {
                        ControlBindingUnavailableReason::SceneAudioNotPrepared
                    }
                };
                ControlBindingAvailability::Unavailable(reason)
            }
            Err(
                TransportCommandSendError::MissingScene { .. }
                | TransportCommandSendError::LoopRangeShorterThanBlock { .. }
                | TransportCommandSendError::Full(_)
                | TransportCommandSendError::Disconnected(_),
            ) => ControlBindingAvailability::Unavailable(
                ControlBindingUnavailableReason::SceneAudioNotPrepared,
            ),
        }
    }

    fn boolean_state(&self, target: BooleanControlTarget) -> Option<bool> {
        let state = self.recovery.snapshot().mixer_parameter_state()?;
        match target {
            BooleanControlTarget::ChannelMute { channel_index } => {
                Some(state.channel(channel_index)?.muted())
            }
            BooleanControlTarget::ChannelSolo { channel_index } => {
                Some(state.channel(channel_index)?.solo())
            }
            BooleanControlTarget::SendEnabled {
                channel_index,
                send_index,
            } => Some(state.channel(channel_index)?.send(send_index)?.enabled()),
            BooleanControlTarget::MainMute => Some(state.main_muted()),
        }
    }

    fn transport_rolling(&self) -> Option<bool> {
        Some(matches!(
            self.recovery.snapshot().state(),
            PlaybackRecoveryTransportState::Rolling
        ))
    }

    fn value_state(&self, target: ValueControlTarget) -> Option<i32> {
        value_from_state(&self.recovery.snapshot().mixer_parameter_state()?, target)
    }

    fn value_domain(&self, target: ValueControlTarget) -> ValueDomain {
        match target {
            ValueControlTarget::ChannelGain { .. }
            | ValueControlTarget::SendGain { .. }
            | ValueControlTarget::MainGain => ValueDomain::new(0, GAIN_MILLIONTHS_MAX),
            ValueControlTarget::ChannelPan { .. } | ValueControlTarget::SendPan { .. } => {
                ValueDomain::new(-PAN_MILLIONTHS_MAX, PAN_MILLIONTHS_MAX)
            }
        }
    }

    fn try_send(&mut self, command: RuntimeControlCommand) -> Result<(), Self::Error> {
        let transport = match command {
            RuntimeControlCommand::TransportStart => Some(TransportCommand::Start),
            RuntimeControlCommand::TransportStop => Some(TransportCommand::Stop),
            RuntimeControlCommand::TransportLocateStartPreserve => Some(TransportCommand::Locate {
                frame: 0,
                state: LocateState::Preserve,
            }),
            RuntimeControlCommand::SceneLaunch { scene_index } => {
                Some(TransportCommand::LaunchScene { scene_index })
            }
            RuntimeControlCommand::ChannelGain { .. }
            | RuntimeControlCommand::ChannelPan { .. }
            | RuntimeControlCommand::ChannelMute { .. }
            | RuntimeControlCommand::ChannelSolo { .. }
            | RuntimeControlCommand::SendGain { .. }
            | RuntimeControlCommand::SendPan { .. }
            | RuntimeControlCommand::SendEnabled { .. }
            | RuntimeControlCommand::MainGain { .. }
            | RuntimeControlCommand::MainMute { .. } => None,
        };
        if let Some(command) = transport {
            return self
                .transport
                .try_send(command)
                .map_err(DesktopControlSinkError::TransportSend);
        }
        let command = self.mixer_command(command)?;
        self.mixer
            .try_send(command)
            .map_err(DesktopControlSinkError::MixerSend)
    }

    fn try_send_direct(
        &mut self,
        command: Self::DirectCommand,
    ) -> Result<DirectControlStateUpdate, Self::DirectError> {
        match command {
            DesktopDirectControlCommand::Mixer(command) => {
                let update = self.direct_mixer_update(command).map_err(|source| {
                    DesktopDirectControlError::Mixer(
                        MixerParameterCommandSendError::InvalidTarget { command, source },
                    )
                })?;
                self.mixer
                    .try_send(command)
                    .map_err(DesktopDirectControlError::Mixer)?;
                Ok(update)
            }
            DesktopDirectControlCommand::Transport(command) => {
                self.transport
                    .try_send(command)
                    .map_err(DesktopDirectControlError::Transport)?;
                Ok(match command {
                    TransportCommand::Start => {
                        DirectControlStateUpdate::Transport { rolling: true }
                    }
                    TransportCommand::Stop => {
                        DirectControlStateUpdate::Transport { rolling: false }
                    }
                    TransportCommand::Locate {
                        state: LocateState::Stopped,
                        ..
                    } => DirectControlStateUpdate::Transport { rolling: false },
                    TransportCommand::Locate {
                        state: LocateState::Rolling,
                        ..
                    } => DirectControlStateUpdate::Transport { rolling: true },
                    TransportCommand::Locate {
                        state: LocateState::Preserve,
                        ..
                    }
                    | TransportCommand::LaunchScene { .. }
                    | TransportCommand::SetLoopRange(_)
                    | TransportCommand::SetLoopEnabled(_) => DirectControlStateUpdate::Unchanged,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use hyper_audio_engine::ChannelParameters;
    use hyper_mixer_runtime::{MixerChannelParameterState, MixerSendParameterState};

    use super::{
        GAIN_MILLIONTHS_MAX, LinearGain, MixerParameterStateSnapshot, PAN_MILLIONTHS_MAX, Pan,
        ValueControlTarget, gain_millionths, value_from_state,
    };

    fn gain(millionths: i32) -> LinearGain {
        LinearGain::from_millionths(u32::try_from(millionths).unwrap()).unwrap()
    }

    fn pan(millionths: i32) -> Pan {
        Pan::from_millionths(millionths).unwrap()
    }

    fn parameters(gain_millionths: i32, pan_millionths: i32) -> ChannelParameters {
        ChannelParameters::new(gain(gain_millionths), pan(pan_millionths), true)
    }

    fn snapshot() -> MixerParameterStateSnapshot {
        MixerParameterStateSnapshot::for_test(
            &[
                MixerChannelParameterState::for_test(
                    parameters(100_000, -100_000),
                    false,
                    false,
                    &[],
                ),
                MixerChannelParameterState::for_test(
                    parameters(200_000, -200_000),
                    false,
                    false,
                    &[
                        MixerSendParameterState::for_test(parameters(300_000, -300_000), true),
                        MixerSendParameterState::for_test(parameters(400_000, -400_000), true),
                    ],
                ),
            ],
            gain(500_000),
            false,
        )
    }

    #[test]
    fn each_value_target_reads_its_own_parameter_from_the_snapshot() {
        let state = snapshot();

        assert_eq!(
            value_from_state(&state, ValueControlTarget::ChannelGain { channel_index: 1 }),
            Some(200_000)
        );
        assert_eq!(
            value_from_state(&state, ValueControlTarget::ChannelPan { channel_index: 1 }),
            Some(-200_000)
        );
        assert_eq!(
            value_from_state(
                &state,
                ValueControlTarget::SendGain {
                    channel_index: 1,
                    send_index: 1,
                }
            ),
            Some(400_000)
        );
        assert_eq!(
            value_from_state(
                &state,
                ValueControlTarget::SendPan {
                    channel_index: 1,
                    send_index: 0,
                }
            ),
            Some(-300_000)
        );
        assert_eq!(
            value_from_state(&state, ValueControlTarget::MainGain),
            Some(500_000)
        );
    }

    #[test]
    fn an_absent_channel_or_send_reads_as_unknown_rather_than_as_a_neighbour() {
        let state = snapshot();

        assert_eq!(
            value_from_state(&state, ValueControlTarget::ChannelGain { channel_index: 2 }),
            None
        );
        assert_eq!(
            value_from_state(
                &state,
                ValueControlTarget::SendGain {
                    channel_index: 0,
                    send_index: 0,
                }
            ),
            None
        );
        assert_eq!(
            value_from_state(
                &state,
                ValueControlTarget::SendPan {
                    channel_index: 1,
                    send_index: 2,
                }
            ),
            None
        );
    }

    #[test]
    fn the_declared_value_domains_are_exactly_what_the_parameter_types_accept() {
        assert!(LinearGain::from_millionths(u32::try_from(GAIN_MILLIONTHS_MAX).unwrap()).is_ok());
        assert!(
            LinearGain::from_millionths(u32::try_from(GAIN_MILLIONTHS_MAX).unwrap() + 1).is_err()
        );
        assert!(Pan::from_millionths(PAN_MILLIONTHS_MAX).is_ok());
        assert!(Pan::from_millionths(PAN_MILLIONTHS_MAX + 1).is_err());
        assert!(Pan::from_millionths(-PAN_MILLIONTHS_MAX).is_ok());
        assert!(Pan::from_millionths(-PAN_MILLIONTHS_MAX - 1).is_err());
    }

    #[test]
    fn a_gain_at_its_ceiling_converts_without_overflowing_the_signed_domain() {
        assert_eq!(
            gain_millionths(gain(GAIN_MILLIONTHS_MAX)),
            GAIN_MILLIONTHS_MAX
        );
        assert_eq!(gain_millionths(LinearGain::SILENCE), 0);
        assert_eq!(gain_millionths(LinearGain::UNITY), 1_000_000);
    }
}
