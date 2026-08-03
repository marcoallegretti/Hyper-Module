use core::error::Error;
use core::fmt;

use hyper_audio_engine::{LinearGain, Pan, ParameterError};
use hyper_control_mappings::{ChannelParameter, MainParameter, SendParameter};
use hyper_control_runtime::{
    BooleanControlTarget, ControlRuntime, RuntimeControlCommand, RuntimeControlSink,
    RuntimeControlTarget,
};
use hyper_mixer_runtime::{
    LocateState, MixerParameterCommand, MixerParameterCommandSendError,
    MixerParameterCommandSender, MixerParameterResolveError, MixerParameterTargetError,
    PlaybackRecoveryCheckpoint, PlaybackRecoveryTransportState, TransportCommand,
    TransportCommandSendError, TransportCommandSender,
};

pub type DesktopControlMappingRuntime = ControlRuntime<DesktopControlSink>;

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
            | RuntimeControlCommand::TransportLocateStartPreserve => {
                Err(DesktopControlSinkError::CommandDomain)
            }
        }
    }
}

impl RuntimeControlSink for DesktopControlSink {
    type Error = DesktopControlSinkError;

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
        };
        let command = self.mixer_command(command)?;
        self.mixer
            .validate_command(command)
            .map_err(DesktopControlSinkError::Target)
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

    fn try_send(&mut self, command: RuntimeControlCommand) -> Result<(), Self::Error> {
        let transport = match command {
            RuntimeControlCommand::TransportStart => Some(TransportCommand::Start),
            RuntimeControlCommand::TransportStop => Some(TransportCommand::Stop),
            RuntimeControlCommand::TransportLocateStartPreserve => Some(TransportCommand::Locate {
                frame: 0,
                state: LocateState::Preserve,
            }),
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
}
