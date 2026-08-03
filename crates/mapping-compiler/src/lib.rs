#![doc = "Validated control-mapping compilation into string-free runtime bindings."]
#![forbid(unsafe_code)]

use core::error::Error;
use core::fmt;

use hyper_control_mappings::{
    ChannelParameter, ContinuousControlTarget, ContinuousControlValue, ControlMappingBindingError,
    ControlMappingDocument, ControlMappingProject, ControlScope, ControlSource, ControlTarget,
    ControlTransform, CueParameter, Edge, MainParameter, SceneAction, SendParameter,
    TransportAction, TriggerBehavior, map_continuous_value,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompiledControlSource {
    CvInput { input_index: u8 },
    ClockInput { edge: Edge },
    ResetInput { edge: Edge },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompiledControlScope {
    Global,
    MixerPage,
    Track { track_index: usize },
    Scene { scene_index: usize },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MixerSendAddress {
    channel_index: usize,
    send_index: usize,
}

impl MixerSendAddress {
    #[must_use]
    pub const fn new(channel_index: usize, send_index: usize) -> Self {
        Self {
            channel_index,
            send_index,
        }
    }

    #[must_use]
    pub const fn channel_index(self) -> usize {
        self.channel_index
    }

    #[must_use]
    pub const fn send_index(self) -> usize {
        self.send_index
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompiledControlTarget {
    MixerChannel {
        channel_index: usize,
        parameter: ChannelParameter,
    },
    MixerSend {
        address: MixerSendAddress,
        parameter: SendParameter,
    },
    MixerMain {
        parameter: MainParameter,
    },
    MixerCue {
        parameter: CueParameter,
    },
    Scene {
        scene_index: usize,
        action: SceneAction,
    },
    Transport {
        action: TransportAction,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompiledControlBinding {
    source_binding_index: usize,
    source: CompiledControlSource,
    scope: CompiledControlScope,
    transform: ControlTransform,
    target: CompiledControlTarget,
}

impl CompiledControlBinding {
    #[must_use]
    pub const fn source_binding_index(self) -> usize {
        self.source_binding_index
    }

    #[must_use]
    pub const fn source(self) -> CompiledControlSource {
        self.source
    }

    #[must_use]
    pub const fn scope(self) -> CompiledControlScope {
        self.scope
    }

    #[must_use]
    pub const fn transform(self) -> ControlTransform {
        self.transform
    }

    #[must_use]
    pub const fn target(self) -> CompiledControlTarget {
        self.target
    }

    #[must_use]
    pub fn map_continuous(self, input: i32) -> Option<ContinuousControlValue> {
        let target = match self.target {
            CompiledControlTarget::MixerChannel {
                parameter: ChannelParameter::Gain,
                ..
            }
            | CompiledControlTarget::MixerSend {
                parameter: SendParameter::Gain,
                ..
            }
            | CompiledControlTarget::MixerMain {
                parameter: MainParameter::Gain,
            }
            | CompiledControlTarget::MixerCue {
                parameter: CueParameter::Gain,
            } => ContinuousControlTarget::Gain,
            CompiledControlTarget::MixerChannel {
                parameter: ChannelParameter::Pan,
                ..
            }
            | CompiledControlTarget::MixerSend {
                parameter: SendParameter::Pan,
                ..
            } => ContinuousControlTarget::Pan,
            CompiledControlTarget::MixerChannel { .. }
            | CompiledControlTarget::MixerSend { .. }
            | CompiledControlTarget::MixerMain { .. }
            | CompiledControlTarget::MixerCue { .. }
            | CompiledControlTarget::Scene { .. }
            | CompiledControlTarget::Transport { .. } => return None,
        };
        map_continuous_value(self.transform, target, input)
    }

    #[must_use]
    pub const fn trigger_behavior(self) -> Option<TriggerBehavior> {
        match self.transform {
            ControlTransform::Trigger { behavior } => Some(behavior),
            ControlTransform::Linear { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompiledControlMappingTable {
    bindings: Vec<CompiledControlBinding>,
}

impl CompiledControlMappingTable {
    #[must_use]
    pub fn bindings(&self) -> &[CompiledControlBinding] {
        &self.bindings
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }
}

pub trait ControlMappingResolver: ControlMappingProject {
    fn track_index(&self, track_id: &str) -> Option<usize>;
    fn scene_index(&self, scene_id: &str) -> Option<usize>;
    fn mixer_channel_index(&self, track_id: &str) -> Option<usize>;
    fn mixer_send_address(&self, send_id: &str) -> Option<MixerSendAddress>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlMappingReference {
    TrackScope,
    SceneScope,
    MixerChannel,
    MixerSend,
    SceneTarget,
}

impl fmt::Display for ControlMappingReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::TrackScope => "track-scope",
            Self::SceneScope => "scene-scope",
            Self::MixerChannel => "mixer-channel",
            Self::MixerSend => "mixer-send",
            Self::SceneTarget => "scene-target",
        })
    }
}

#[derive(Debug)]
pub enum ControlMappingCompileError {
    Binding(ControlMappingBindingError),
    CapacityUnavailable,
    ResolverInconsistent {
        binding_index: usize,
        reference: ControlMappingReference,
    },
}

impl fmt::Display for ControlMappingCompileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Binding(source) => source.fmt(formatter),
            Self::CapacityUnavailable => {
                formatter.write_str("compiled control-mapping capacity is unavailable")
            }
            Self::ResolverInconsistent {
                binding_index,
                reference,
            } => write!(
                formatter,
                "validated control binding {binding_index} has no {reference} runtime address"
            ),
        }
    }
}

impl Error for ControlMappingCompileError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Binding(source) => Some(source),
            Self::CapacityUnavailable | Self::ResolverInconsistent { .. } => None,
        }
    }
}

impl From<ControlMappingBindingError> for ControlMappingCompileError {
    fn from(source: ControlMappingBindingError) -> Self {
        Self::Binding(source)
    }
}

/// Validates and compiles one mapping document into active, string-free runtime bindings.
///
/// Disabled bindings remain in the editable document and are intentionally absent from the
/// runtime table. Each compiled binding retains its source-document ordinal for diagnostics.
///
/// # Errors
///
/// Returns a binding rejection, allocation failure, or a fail-closed resolver inconsistency.
pub fn compile_control_mapping(
    document: &ControlMappingDocument,
    descriptor_id: &str,
    resolver: &impl ControlMappingResolver,
) -> Result<CompiledControlMappingTable, ControlMappingCompileError> {
    document.validate_for_project(descriptor_id, resolver)?;
    let active_count = document
        .bindings()
        .iter()
        .filter(|binding| binding.enabled())
        .count();
    let mut bindings = Vec::new();
    bindings
        .try_reserve_exact(active_count)
        .map_err(|_| ControlMappingCompileError::CapacityUnavailable)?;
    for (binding_index, binding) in document.bindings().iter().enumerate() {
        if !binding.enabled() {
            continue;
        }
        let source = match binding.source() {
            ControlSource::CvInput { input_index } => CompiledControlSource::CvInput {
                input_index: *input_index,
            },
            ControlSource::ClockInput { edge } => CompiledControlSource::ClockInput { edge: *edge },
            ControlSource::ResetInput { edge } => CompiledControlSource::ResetInput { edge: *edge },
        };
        let scope = compile_scope(binding.scope(), binding_index, resolver)?;
        let target = compile_target(binding.target(), binding_index, resolver)?;
        bindings.push(CompiledControlBinding {
            source_binding_index: binding_index,
            source,
            scope,
            transform: binding.transform(),
            target,
        });
    }
    Ok(CompiledControlMappingTable { bindings })
}

fn compile_scope(
    scope: &ControlScope,
    binding_index: usize,
    resolver: &impl ControlMappingResolver,
) -> Result<CompiledControlScope, ControlMappingCompileError> {
    match scope {
        ControlScope::Global => Ok(CompiledControlScope::Global),
        ControlScope::MixerPage => Ok(CompiledControlScope::MixerPage),
        ControlScope::Track { track_id } => resolver
            .track_index(track_id)
            .map(|track_index| CompiledControlScope::Track { track_index })
            .ok_or(ControlMappingCompileError::ResolverInconsistent {
                binding_index,
                reference: ControlMappingReference::TrackScope,
            }),
        ControlScope::Scene { scene_id } => resolver
            .scene_index(scene_id)
            .map(|scene_index| CompiledControlScope::Scene { scene_index })
            .ok_or(ControlMappingCompileError::ResolverInconsistent {
                binding_index,
                reference: ControlMappingReference::SceneScope,
            }),
    }
}

fn compile_target(
    target: &ControlTarget,
    binding_index: usize,
    resolver: &impl ControlMappingResolver,
) -> Result<CompiledControlTarget, ControlMappingCompileError> {
    match target {
        ControlTarget::MixerChannel {
            track_id,
            parameter,
        } => resolver
            .mixer_channel_index(track_id)
            .map(|channel_index| CompiledControlTarget::MixerChannel {
                channel_index,
                parameter: *parameter,
            })
            .ok_or(ControlMappingCompileError::ResolverInconsistent {
                binding_index,
                reference: ControlMappingReference::MixerChannel,
            }),
        ControlTarget::MixerSend { send_id, parameter } => resolver
            .mixer_send_address(send_id)
            .map(|address| CompiledControlTarget::MixerSend {
                address,
                parameter: *parameter,
            })
            .ok_or(ControlMappingCompileError::ResolverInconsistent {
                binding_index,
                reference: ControlMappingReference::MixerSend,
            }),
        ControlTarget::MixerMain { parameter } => Ok(CompiledControlTarget::MixerMain {
            parameter: *parameter,
        }),
        ControlTarget::MixerCue { parameter } => Ok(CompiledControlTarget::MixerCue {
            parameter: *parameter,
        }),
        ControlTarget::Scene { scene_id, action } => resolver
            .scene_index(scene_id)
            .map(|scene_index| CompiledControlTarget::Scene {
                scene_index,
                action: *action,
            })
            .ok_or(ControlMappingCompileError::ResolverInconsistent {
                binding_index,
                reference: ControlMappingReference::SceneTarget,
            }),
        ControlTarget::Transport { action } => {
            Ok(CompiledControlTarget::Transport { action: *action })
        }
    }
}
