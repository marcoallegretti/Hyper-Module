#![doc = "Strict, typed version-1 control-mapping authority."]
#![forbid(unsafe_code)]

use core::error::Error;
use core::fmt;
use std::collections::BTreeMap;

use hyper_project_model::{is_valid_entity_id, is_valid_extension_key};
use hyper_strict_json::{StrictJsonError, StrictJsonErrorKind};
use serde::Deserialize;
use serde_json::Value;

const FORMAT: &str = "hyper-control-mappings";
const FORMAT_VERSION: u8 = 1;
const MAX_DOCUMENT_BYTES: usize = 1_048_576;
const MAX_BINDINGS: usize = 4_096;
const NORMALIZED_SIGNED_MIN: i32 = -1_000_000;
const NORMALIZED_SIGNED_MAX: i32 = 1_000_000;
const NORMALIZED_UNSIGNED_MAX: i32 = 1_000_000;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "lowercase")]
pub enum Edge {
    Rising,
    Falling,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ControlSource {
    CvInput { input_index: u8 },
    ClockInput { edge: Edge },
    ResetInput { edge: Edge },
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ControlScope {
    Global,
    MixerPage,
    Track { track_id: String },
    Scene { scene_id: String },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum TriggerBehavior {
    Invoke,
    Toggle,
    Momentary,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ControlTransform {
    Linear {
        input_min: i32,
        input_max: i32,
        output_min: i32,
        output_max: i32,
    },
    Trigger {
        behavior: TriggerBehavior,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum ChannelParameter {
    Gain,
    Pan,
    Mute,
    Solo,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum SendParameter {
    Gain,
    Pan,
    Enabled,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum MainParameter {
    Gain,
    Mute,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum CueParameter {
    Gain,
    Mute,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ControlTarget {
    MixerChannel {
        track_id: String,
        parameter: ChannelParameter,
    },
    MixerSend {
        send_id: String,
        parameter: SendParameter,
    },
    MixerMain {
        parameter: MainParameter,
    },
    MixerCue {
        parameter: CueParameter,
    },
    Scene {
        scene_id: String,
        action: SceneAction,
    },
    Transport {
        action: TransportAction,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SceneAction {
    Launch,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum TransportAction {
    Play,
    Stop,
    Toggle,
    Reset,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContinuousControlValue {
    GainMillionths(u32),
    PanMillionths(i32),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ControlBinding {
    id: String,
    enabled: bool,
    source: ControlSource,
    scope: ControlScope,
    transform: ControlTransform,
    target: ControlTarget,
}

impl ControlBinding {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    #[must_use]
    pub const fn source(&self) -> &ControlSource {
        &self.source
    }

    #[must_use]
    pub const fn scope(&self) -> &ControlScope {
        &self.scope
    }

    #[must_use]
    pub const fn transform(&self) -> ControlTransform {
        self.transform
    }

    #[must_use]
    pub const fn target(&self) -> &ControlTarget {
        &self.target
    }

    /// Applies a validated CV binding's exact integer transform and target
    /// domain conversion. Trigger bindings return `None`.
    #[must_use]
    pub fn map_continuous(&self, input: i32) -> Option<ContinuousControlValue> {
        let ControlTransform::Linear {
            input_min,
            input_max,
            output_min,
            output_max,
        } = self.transform
        else {
            return None;
        };
        let clamped = input.clamp(input_min, input_max);
        let numerator = i64::from(clamped - input_min) * i64::from(output_max - output_min);
        let denominator = i64::from(input_max - input_min);
        let normalized = i64::from(output_min) + numerator / denominator;
        let normalized = i32::try_from(normalized).ok()?;
        match self.target {
            ControlTarget::MixerChannel {
                parameter: ChannelParameter::Gain,
                ..
            }
            | ControlTarget::MixerSend {
                parameter: SendParameter::Gain,
                ..
            }
            | ControlTarget::MixerMain {
                parameter: MainParameter::Gain,
            }
            | ControlTarget::MixerCue {
                parameter: CueParameter::Gain,
            } => Some(ContinuousControlValue::GainMillionths(
                u32::try_from(normalized).ok()?.checked_mul(2)?,
            )),
            ControlTarget::MixerChannel {
                parameter: ChannelParameter::Pan,
                ..
            }
            | ControlTarget::MixerSend {
                parameter: SendParameter::Pan,
                ..
            } => Some(ContinuousControlValue::PanMillionths(
                normalized.checked_mul(2)?.checked_sub(1_000_000)?,
            )),
            ControlTarget::MixerChannel { .. }
            | ControlTarget::MixerSend { .. }
            | ControlTarget::MixerMain { .. }
            | ControlTarget::MixerCue { .. }
            | ControlTarget::Scene { .. }
            | ControlTarget::Transport { .. } => None,
        }
    }

    #[must_use]
    pub const fn trigger_behavior(&self) -> Option<TriggerBehavior> {
        match self.transform {
            ControlTransform::Trigger { behavior } => Some(behavior),
            ControlTransform::Linear { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlMappingDocument {
    id: String,
    project_id: String,
    bindings: Vec<ControlBinding>,
    extensions: BTreeMap<String, Value>,
}

impl ControlMappingDocument {
    /// Parses and validates one complete version-1 mapping document.
    ///
    /// Project capabilities and cross-resource references are validated by
    /// [`Self::validate_for_project`] after the surrounding package is loaded.
    ///
    /// # Errors
    ///
    /// Returns a strict JSON, schema-domain, ordering, uniqueness, transform,
    /// or target-domain rejection.
    pub fn parse(bytes: &[u8]) -> Result<Self, ControlMappingParseError> {
        let raw: RawControlMappingDocument = hyper_strict_json::parse(bytes, MAX_DOCUMENT_BYTES)?;
        validate_document(raw)
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    #[must_use]
    pub fn bindings(&self) -> &[ControlBinding] {
        &self.bindings
    }

    #[must_use]
    pub const fn extensions(&self) -> &BTreeMap<String, Value> {
        &self.extensions
    }

    /// Binds every mapping identity, capability, scope, and target reference
    /// to one already validated project graph.
    ///
    /// # Errors
    ///
    /// Returns the binding identity and a classified reason when the mapping
    /// document cannot be executed by the supplied project authority.
    pub fn validate_for_project(
        &self,
        descriptor_id: &str,
        project: &impl ControlMappingProject,
    ) -> Result<(), ControlMappingBindingError> {
        if self.id != descriptor_id {
            return Err(binding_error(
                None,
                ControlMappingBindingErrorKind::DescriptorIdMismatch,
            ));
        }
        if self.project_id != project.project_id() {
            return Err(binding_error(
                None,
                ControlMappingBindingErrorKind::ProjectIdMismatch,
            ));
        }
        if project.contains_nested_entity(&self.id) {
            return Err(binding_error(
                None,
                ControlMappingBindingErrorKind::DocumentIdentityCollision,
            ));
        }
        for binding in &self.bindings {
            validate_project_binding(binding, project)?;
        }
        Ok(())
    }
}

pub trait ControlMappingProject {
    fn project_id(&self) -> &str;
    fn cv_inputs(&self) -> u8;
    fn has_clock_input(&self) -> bool;
    fn has_reset_input(&self) -> bool;
    fn has_mixer(&self) -> bool;
    fn contains_nested_entity(&self, id: &str) -> bool;
    fn contains_entity(&self, id: &str) -> bool;
    fn contains_track(&self, id: &str) -> bool;
    fn contains_scene(&self, id: &str) -> bool;
    fn contains_mixer_channel(&self, track_id: &str) -> bool;
    fn contains_mixer_send(&self, send_id: &str) -> bool;
    fn is_solo_source_track(&self, track_id: &str) -> bool;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlMappingParseErrorKind {
    Json(StrictJsonErrorKind),
    InvalidDomain,
    CapacityUnavailable,
}

#[derive(Debug)]
pub struct ControlMappingParseError {
    kind: ControlMappingParseErrorKind,
    message: String,
    source: Option<StrictJsonError>,
}

impl ControlMappingParseError {
    #[must_use]
    pub const fn kind(&self) -> ControlMappingParseErrorKind {
        self.kind
    }
}

impl fmt::Display for ControlMappingParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ControlMappingParseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_ref().map(|source| source as _)
    }
}

impl From<StrictJsonError> for ControlMappingParseError {
    fn from(error: StrictJsonError) -> Self {
        Self {
            kind: ControlMappingParseErrorKind::Json(error.kind),
            message: error.message.clone(),
            source: Some(error),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlMappingBindingErrorKind {
    DescriptorIdMismatch,
    ProjectIdMismatch,
    DocumentIdentityCollision,
    IdentityCollision,
    CvInputUnavailable { index: u8, available: u8 },
    ClockInputUnavailable,
    ResetInputUnavailable,
    TrackScopeMissing,
    SceneScopeMissing,
    MixerScopeMissing,
    MixerChannelMissing,
    MixerSendMissing,
    MixerTargetMissing,
    SceneTargetMissing,
    SoloTargetNotSource,
}

#[derive(Debug)]
pub struct ControlMappingBindingError {
    binding_id: Option<String>,
    kind: ControlMappingBindingErrorKind,
}

impl ControlMappingBindingError {
    #[must_use]
    pub fn binding_id(&self) -> Option<&str> {
        self.binding_id.as_deref()
    }

    #[must_use]
    pub const fn kind(&self) -> ControlMappingBindingErrorKind {
        self.kind
    }
}

impl fmt::Display for ControlMappingBindingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(binding_id) = &self.binding_id {
            write!(formatter, "control binding {binding_id}: ")?;
        }
        match self.kind {
            ControlMappingBindingErrorKind::DescriptorIdMismatch => {
                formatter.write_str("document ID does not match its resource descriptor")
            }
            ControlMappingBindingErrorKind::ProjectIdMismatch => {
                formatter.write_str("document belongs to another project")
            }
            ControlMappingBindingErrorKind::DocumentIdentityCollision => {
                formatter.write_str("document ID collides with a nested project entity")
            }
            ControlMappingBindingErrorKind::IdentityCollision => {
                formatter.write_str("identifier collides with another project entity")
            }
            ControlMappingBindingErrorKind::CvInputUnavailable { index, available } => write!(
                formatter,
                "CV input {index} is unavailable when the project declares {available} inputs"
            ),
            ControlMappingBindingErrorKind::ClockInputUnavailable => {
                formatter.write_str("Clock input is unavailable")
            }
            ControlMappingBindingErrorKind::ResetInputUnavailable => {
                formatter.write_str("Reset input is unavailable")
            }
            ControlMappingBindingErrorKind::TrackScopeMissing => {
                formatter.write_str("track scope does not exist")
            }
            ControlMappingBindingErrorKind::SceneScopeMissing => {
                formatter.write_str("scene scope does not exist")
            }
            ControlMappingBindingErrorKind::MixerScopeMissing => {
                formatter.write_str("mixer-page scope requires a mixer")
            }
            ControlMappingBindingErrorKind::MixerChannelMissing => {
                formatter.write_str("target mixer channel does not exist")
            }
            ControlMappingBindingErrorKind::MixerSendMissing => {
                formatter.write_str("target mixer send does not exist")
            }
            ControlMappingBindingErrorKind::MixerTargetMissing => {
                formatter.write_str("target requires a mixer")
            }
            ControlMappingBindingErrorKind::SceneTargetMissing => {
                formatter.write_str("target scene does not exist")
            }
            ControlMappingBindingErrorKind::SoloTargetNotSource => {
                formatter.write_str("solo target is not a source track")
            }
        }
    }
}

impl Error for ControlMappingBindingError {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawControlMappingDocument {
    format: String,
    format_version: u8,
    id: String,
    project_id: String,
    bindings: Vec<ControlBinding>,
    extensions: BTreeMap<String, Value>,
}

fn validate_document(
    raw: RawControlMappingDocument,
) -> Result<ControlMappingDocument, ControlMappingParseError> {
    require(raw.format == FORMAT, "control mapping format is invalid")?;
    require(
        raw.format_version == FORMAT_VERSION,
        "control mapping format version is unsupported",
    )?;
    require(is_valid_entity_id(&raw.id), "control mapping ID is invalid")?;
    require(
        is_valid_entity_id(&raw.project_id),
        "control mapping project ID is invalid",
    )?;
    require(
        raw.bindings.len() <= MAX_BINDINGS,
        "control mapping binding count exceeds the version 1 limit",
    )?;
    require(
        raw.extensions.keys().all(|key| is_valid_extension_key(key)),
        "control mapping extensions contain an invalid key",
    )?;

    let mut previous_id: Option<&str> = None;
    for binding in &raw.bindings {
        require(
            is_valid_entity_id(&binding.id),
            "control binding ID is invalid",
        )?;
        require(
            previous_id.is_none_or(|previous| previous < binding.id.as_str()),
            "control bindings are not strictly sorted by unique identifier",
        )?;
        previous_id = Some(&binding.id);
        validate_binding_domain(binding)?;
    }

    let mut by_source_scope = Vec::new();
    by_source_scope
        .try_reserve_exact(raw.bindings.len())
        .map_err(|_| {
            parse_error(
                ControlMappingParseErrorKind::CapacityUnavailable,
                "control mapping uniqueness capacity is unavailable",
            )
        })?;
    by_source_scope.extend(raw.bindings.iter());
    by_source_scope.sort_unstable_by(|left, right| {
        left.source
            .cmp(&right.source)
            .then_with(|| left.scope.cmp(&right.scope))
    });
    require(
        by_source_scope
            .windows(2)
            .all(|pair| pair[0].source != pair[1].source || pair[0].scope != pair[1].scope),
        "control source and scope combinations are not unique",
    )?;

    Ok(ControlMappingDocument {
        id: raw.id,
        project_id: raw.project_id,
        bindings: raw.bindings,
        extensions: raw.extensions,
    })
}

fn validate_binding_domain(binding: &ControlBinding) -> Result<(), ControlMappingParseError> {
    match &binding.source {
        ControlSource::CvInput { input_index } => {
            require(
                *input_index < 4,
                "CV input index exceeds the version 1 domain",
            )?;
            let ControlTransform::Linear {
                input_min,
                input_max,
                output_min,
                output_max,
            } = binding.transform
            else {
                return Err(domain_error("CV sources require a linear transform"));
            };
            require(
                (NORMALIZED_SIGNED_MIN..=NORMALIZED_SIGNED_MAX).contains(&input_min)
                    && (NORMALIZED_SIGNED_MIN..=NORMALIZED_SIGNED_MAX).contains(&input_max)
                    && input_min < input_max,
                "linear input range is invalid",
            )?;
            require(
                (0..=NORMALIZED_UNSIGNED_MAX).contains(&output_min)
                    && (0..=NORMALIZED_UNSIGNED_MAX).contains(&output_max),
                "linear output range is invalid",
            )?;
            require(
                target_domain(&binding.target) == TargetDomain::Continuous,
                "CV sources require a continuous target",
            )?;
        }
        ControlSource::ClockInput { .. } | ControlSource::ResetInput { .. } => {
            let ControlTransform::Trigger { behavior } = binding.transform else {
                return Err(domain_error("edge sources require a trigger transform"));
            };
            match target_domain(&binding.target) {
                TargetDomain::Action => require(
                    behavior == TriggerBehavior::Invoke,
                    "action targets require invoke behavior",
                )?,
                TargetDomain::Boolean => require(
                    matches!(
                        behavior,
                        TriggerBehavior::Toggle | TriggerBehavior::Momentary
                    ),
                    "Boolean targets require toggle or momentary behavior",
                )?,
                TargetDomain::Continuous => {
                    return Err(domain_error(
                        "edge sources require a Boolean or action target",
                    ));
                }
            }
        }
    }
    validate_scope_ids(&binding.scope)?;
    validate_target_ids(&binding.target)
}

fn validate_scope_ids(scope: &ControlScope) -> Result<(), ControlMappingParseError> {
    match scope {
        ControlScope::Track { track_id } => require(
            is_valid_entity_id(track_id),
            "control mapping track scope ID is invalid",
        ),
        ControlScope::Scene { scene_id } => require(
            is_valid_entity_id(scene_id),
            "control mapping scene scope ID is invalid",
        ),
        ControlScope::Global | ControlScope::MixerPage => Ok(()),
    }
}

fn validate_target_ids(target: &ControlTarget) -> Result<(), ControlMappingParseError> {
    match target {
        ControlTarget::MixerChannel { track_id, .. } => require(
            is_valid_entity_id(track_id),
            "control mapping channel target ID is invalid",
        ),
        ControlTarget::MixerSend { send_id, .. } => require(
            is_valid_entity_id(send_id),
            "control mapping send target ID is invalid",
        ),
        ControlTarget::Scene { scene_id, .. } => require(
            is_valid_entity_id(scene_id),
            "control mapping scene target ID is invalid",
        ),
        ControlTarget::MixerMain { .. }
        | ControlTarget::MixerCue { .. }
        | ControlTarget::Transport { .. } => Ok(()),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TargetDomain {
    Continuous,
    Boolean,
    Action,
}

fn target_domain(target: &ControlTarget) -> TargetDomain {
    match target {
        ControlTarget::MixerChannel { parameter, .. } => match parameter {
            ChannelParameter::Gain | ChannelParameter::Pan => TargetDomain::Continuous,
            ChannelParameter::Mute | ChannelParameter::Solo => TargetDomain::Boolean,
        },
        ControlTarget::MixerSend { parameter, .. } => match parameter {
            SendParameter::Gain | SendParameter::Pan => TargetDomain::Continuous,
            SendParameter::Enabled => TargetDomain::Boolean,
        },
        ControlTarget::MixerMain { parameter } => match parameter {
            MainParameter::Gain => TargetDomain::Continuous,
            MainParameter::Mute => TargetDomain::Boolean,
        },
        ControlTarget::MixerCue { parameter } => match parameter {
            CueParameter::Gain => TargetDomain::Continuous,
            CueParameter::Mute => TargetDomain::Boolean,
        },
        ControlTarget::Scene { .. } | ControlTarget::Transport { .. } => TargetDomain::Action,
    }
}

fn validate_project_binding(
    binding: &ControlBinding,
    project: &impl ControlMappingProject,
) -> Result<(), ControlMappingBindingError> {
    if project.contains_entity(&binding.id) {
        return Err(binding_error(
            Some(&binding.id),
            ControlMappingBindingErrorKind::IdentityCollision,
        ));
    }
    validate_project_source(binding, project)?;
    validate_project_scope(binding, project)?;
    validate_project_target(binding, project)
}

fn validate_project_source(
    binding: &ControlBinding,
    project: &impl ControlMappingProject,
) -> Result<(), ControlMappingBindingError> {
    match binding.source {
        ControlSource::CvInput { input_index } if input_index >= project.cv_inputs() => {
            return Err(binding_error(
                Some(&binding.id),
                ControlMappingBindingErrorKind::CvInputUnavailable {
                    index: input_index,
                    available: project.cv_inputs(),
                },
            ));
        }
        ControlSource::ClockInput { .. } if !project.has_clock_input() => {
            return Err(binding_error(
                Some(&binding.id),
                ControlMappingBindingErrorKind::ClockInputUnavailable,
            ));
        }
        ControlSource::ResetInput { .. } if !project.has_reset_input() => {
            return Err(binding_error(
                Some(&binding.id),
                ControlMappingBindingErrorKind::ResetInputUnavailable,
            ));
        }
        ControlSource::CvInput { .. }
        | ControlSource::ClockInput { .. }
        | ControlSource::ResetInput { .. } => {}
    }
    Ok(())
}

fn validate_project_scope(
    binding: &ControlBinding,
    project: &impl ControlMappingProject,
) -> Result<(), ControlMappingBindingError> {
    match &binding.scope {
        ControlScope::Track { track_id } if !project.contains_track(track_id) => {
            return Err(binding_error(
                Some(&binding.id),
                ControlMappingBindingErrorKind::TrackScopeMissing,
            ));
        }
        ControlScope::Scene { scene_id } if !project.contains_scene(scene_id) => {
            return Err(binding_error(
                Some(&binding.id),
                ControlMappingBindingErrorKind::SceneScopeMissing,
            ));
        }
        ControlScope::MixerPage if !project.has_mixer() => {
            return Err(binding_error(
                Some(&binding.id),
                ControlMappingBindingErrorKind::MixerScopeMissing,
            ));
        }
        ControlScope::Global
        | ControlScope::MixerPage
        | ControlScope::Track { .. }
        | ControlScope::Scene { .. } => {}
    }
    Ok(())
}

fn validate_project_target(
    binding: &ControlBinding,
    project: &impl ControlMappingProject,
) -> Result<(), ControlMappingBindingError> {
    match &binding.target {
        ControlTarget::MixerChannel {
            track_id,
            parameter,
        } => {
            if !project.contains_mixer_channel(track_id) {
                return Err(binding_error(
                    Some(&binding.id),
                    ControlMappingBindingErrorKind::MixerChannelMissing,
                ));
            }
            if *parameter == ChannelParameter::Solo && !project.is_solo_source_track(track_id) {
                return Err(binding_error(
                    Some(&binding.id),
                    ControlMappingBindingErrorKind::SoloTargetNotSource,
                ));
            }
        }
        ControlTarget::MixerSend { send_id, .. } if !project.contains_mixer_send(send_id) => {
            return Err(binding_error(
                Some(&binding.id),
                ControlMappingBindingErrorKind::MixerSendMissing,
            ));
        }
        ControlTarget::Scene { scene_id, .. } if !project.contains_scene(scene_id) => {
            return Err(binding_error(
                Some(&binding.id),
                ControlMappingBindingErrorKind::SceneTargetMissing,
            ));
        }
        ControlTarget::MixerMain { .. } | ControlTarget::MixerCue { .. }
            if !project.has_mixer() =>
        {
            return Err(binding_error(
                Some(&binding.id),
                ControlMappingBindingErrorKind::MixerTargetMissing,
            ));
        }
        ControlTarget::MixerSend { .. }
        | ControlTarget::MixerMain { .. }
        | ControlTarget::MixerCue { .. }
        | ControlTarget::Scene { .. }
        | ControlTarget::Transport { .. } => {}
    }
    Ok(())
}

fn require(condition: bool, message: &str) -> Result<(), ControlMappingParseError> {
    if condition {
        Ok(())
    } else {
        Err(domain_error(message))
    }
}

fn domain_error(message: &str) -> ControlMappingParseError {
    parse_error(ControlMappingParseErrorKind::InvalidDomain, message)
}

fn parse_error(kind: ControlMappingParseErrorKind, message: &str) -> ControlMappingParseError {
    ControlMappingParseError {
        kind,
        message: message.to_owned(),
        source: None,
    }
}

fn binding_error(
    binding_id: Option<&str>,
    kind: ControlMappingBindingErrorKind,
) -> ControlMappingBindingError {
    ControlMappingBindingError {
        binding_id: binding_id.map(str::to_owned),
        kind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeSet;

    const VALID: &[u8] = include_bytes!("../../../contracts/fixtures/valid/control-mappings.json");
    const INVALID_ACTION_LINEAR: &[u8] =
        include_bytes!("../../../contracts/fixtures/invalid/control-mappings-action-linear.json");
    const TRANSFORM_VECTORS: &[u8] =
        include_bytes!("../../../contracts/fixtures/vectors/control-mapping-transforms.json");
    const MAPPING_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334474";
    const PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334455";
    const BINDING_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334475";
    const SECOND_BINDING_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334476";
    const TRACK_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334456";
    const SCENE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334477";
    const SEND_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334478";
    const FOREIGN_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334479";

    #[derive(Deserialize)]
    struct TransformVectorDocument {
        format: String,
        format_version: u8,
        cases: Vec<TransformVector>,
    }

    #[derive(Deserialize)]
    struct TransformVector {
        name: String,
        transform: ControlTransform,
        input: i32,
        normalized: i32,
        gain_millionths: u32,
        pan_millionths: i32,
    }

    #[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
    enum ProjectFact {
        Clock,
        Reset,
        Mixer,
        Track,
        Scene,
        MixerChannel,
        MixerSend,
        SoloSource,
    }

    #[derive(Clone)]
    struct TestProject {
        project_id: &'static str,
        cv_inputs: u8,
        nested_entity: Option<&'static str>,
        entity: Option<&'static str>,
        facts: BTreeSet<ProjectFact>,
    }

    impl Default for TestProject {
        fn default() -> Self {
            Self {
                project_id: PROJECT_ID,
                cv_inputs: 4,
                nested_entity: None,
                entity: None,
                facts: BTreeSet::from([
                    ProjectFact::Clock,
                    ProjectFact::Reset,
                    ProjectFact::Mixer,
                    ProjectFact::Track,
                    ProjectFact::Scene,
                    ProjectFact::MixerChannel,
                    ProjectFact::MixerSend,
                    ProjectFact::SoloSource,
                ]),
            }
        }
    }

    impl TestProject {
        fn without(mut self, fact: ProjectFact) -> Self {
            self.facts.remove(&fact);
            self
        }

        fn with_project_id(mut self, project_id: &'static str) -> Self {
            self.project_id = project_id;
            self
        }

        fn with_cv_inputs(mut self, cv_inputs: u8) -> Self {
            self.cv_inputs = cv_inputs;
            self
        }

        fn with_nested_entity(mut self, id: &'static str) -> Self {
            self.nested_entity = Some(id);
            self
        }

        fn with_entity(mut self, id: &'static str) -> Self {
            self.entity = Some(id);
            self
        }
    }

    impl ControlMappingProject for TestProject {
        fn project_id(&self) -> &str {
            self.project_id
        }

        fn cv_inputs(&self) -> u8 {
            self.cv_inputs
        }

        fn has_clock_input(&self) -> bool {
            self.facts.contains(&ProjectFact::Clock)
        }

        fn has_reset_input(&self) -> bool {
            self.facts.contains(&ProjectFact::Reset)
        }

        fn has_mixer(&self) -> bool {
            self.facts.contains(&ProjectFact::Mixer)
        }

        fn contains_nested_entity(&self, id: &str) -> bool {
            self.nested_entity == Some(id)
        }

        fn contains_entity(&self, id: &str) -> bool {
            self.entity == Some(id)
        }

        fn contains_track(&self, id: &str) -> bool {
            self.facts.contains(&ProjectFact::Track) && id == TRACK_ID
        }

        fn contains_scene(&self, id: &str) -> bool {
            self.facts.contains(&ProjectFact::Scene) && id == SCENE_ID
        }

        fn contains_mixer_channel(&self, track_id: &str) -> bool {
            self.facts.contains(&ProjectFact::MixerChannel) && track_id == TRACK_ID
        }

        fn contains_mixer_send(&self, send_id: &str) -> bool {
            self.facts.contains(&ProjectFact::MixerSend) && send_id == SEND_ID
        }

        fn is_solo_source_track(&self, track_id: &str) -> bool {
            self.facts.contains(&ProjectFact::SoloSource) && track_id == TRACK_ID
        }
    }

    #[test]
    fn parses_the_published_fixture_and_matches_integer_vectors() {
        let document = ControlMappingDocument::parse(VALID).unwrap();
        assert_eq!(document.bindings().len(), 1);
        let binding = &document.bindings()[0];
        assert_eq!(
            binding.map_continuous(-2_000_000),
            Some(ContinuousControlValue::GainMillionths(0))
        );
        assert_eq!(
            binding.map_continuous(0),
            Some(ContinuousControlValue::GainMillionths(1_000_000))
        );
        assert_eq!(
            binding.map_continuous(2_000_000),
            Some(ContinuousControlValue::GainMillionths(2_000_000))
        );
    }

    #[test]
    fn rejects_the_published_cross_domain_fixture() {
        assert!(matches!(
            ControlMappingDocument::parse(INVALID_ACTION_LINEAR),
            Err(error) if error.kind() == ControlMappingParseErrorKind::InvalidDomain
        ));
    }

    #[test]
    fn exact_mapping_supports_reversed_outputs_and_truncates_toward_zero() {
        let bytes = VALID.to_vec();
        let mut value: Value = serde_json::from_slice(&bytes).unwrap();
        value["bindings"][0]["transform"]["input_min"] = (-3).into();
        value["bindings"][0]["transform"]["input_max"] = 3.into();
        value["bindings"][0]["transform"]["output_min"] = 1_000_000.into();
        value["bindings"][0]["transform"]["output_max"] = 0.into();
        value["bindings"][0]["target"]["parameter"] = "pan".into();
        let bytes = serde_json::to_vec(&value).unwrap();
        let binding = &ControlMappingDocument::parse(&bytes).unwrap().bindings[0];
        assert_eq!(
            binding.map_continuous(-2),
            Some(ContinuousControlValue::PanMillionths(666_668))
        );
        assert_eq!(
            binding.map_continuous(2),
            Some(ContinuousControlValue::PanMillionths(-666_666))
        );
    }

    #[test]
    fn shared_transform_vectors_match_every_continuous_target_domain() {
        let vectors: TransformVectorDocument = serde_json::from_slice(TRANSFORM_VECTORS).unwrap();
        assert_eq!(vectors.format, "hyper-control-mapping-transform-vectors");
        assert_eq!(vectors.format_version, 1);
        let mut names = BTreeSet::new();
        for vector in vectors.cases {
            assert!(names.insert(vector.name.clone()));
            assert_eq!(
                vector.normalized.checked_mul(2),
                Some(i32::try_from(vector.gain_millionths).unwrap())
            );
            let gain = continuous_binding(
                vector.transform,
                ControlTarget::MixerMain {
                    parameter: MainParameter::Gain,
                },
            );
            assert_eq!(
                gain.map_continuous(vector.input),
                Some(ContinuousControlValue::GainMillionths(
                    vector.gain_millionths
                )),
                "{}",
                vector.name
            );
            let pan = continuous_binding(
                vector.transform,
                ControlTarget::MixerChannel {
                    track_id: TRACK_ID.to_owned(),
                    parameter: ChannelParameter::Pan,
                },
            );
            assert_eq!(
                pan.map_continuous(vector.input),
                Some(ContinuousControlValue::PanMillionths(vector.pan_millionths)),
                "{}",
                vector.name
            );
        }
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one exhaustive table keeps every published tagged variant visible together"
    )]
    fn parses_every_published_source_scope_transform_and_target_variant() {
        let linear = json!({
            "kind": "linear",
            "input_min": -1_000_000,
            "input_max": 1_000_000,
            "output_min": 0,
            "output_max": 1_000_000
        });
        let cases = [
            (
                json!({"kind": "cv-input", "input_index": 0}),
                json!({"kind": "global"}),
                linear.clone(),
                json!({"kind": "mixer-channel", "track_id": TRACK_ID, "parameter": "gain"}),
            ),
            (
                json!({"kind": "cv-input", "input_index": 3}),
                json!({"kind": "track", "track_id": TRACK_ID}),
                linear.clone(),
                json!({"kind": "mixer-channel", "track_id": TRACK_ID, "parameter": "pan"}),
            ),
            (
                json!({"kind": "cv-input", "input_index": 1}),
                json!({"kind": "scene", "scene_id": SCENE_ID}),
                linear.clone(),
                json!({"kind": "mixer-send", "send_id": SEND_ID, "parameter": "gain"}),
            ),
            (
                json!({"kind": "cv-input", "input_index": 2}),
                json!({"kind": "mixer-page"}),
                linear.clone(),
                json!({"kind": "mixer-send", "send_id": SEND_ID, "parameter": "pan"}),
            ),
            (
                json!({"kind": "cv-input", "input_index": 0}),
                json!({"kind": "global"}),
                linear.clone(),
                json!({"kind": "mixer-main", "parameter": "gain"}),
            ),
            (
                json!({"kind": "cv-input", "input_index": 0}),
                json!({"kind": "mixer-page"}),
                linear,
                json!({"kind": "mixer-cue", "parameter": "gain"}),
            ),
            trigger_case(
                "clock-input",
                "rising",
                "global",
                "toggle",
                json!({"kind": "mixer-channel", "track_id": TRACK_ID, "parameter": "mute"}),
            ),
            trigger_case(
                "clock-input",
                "falling",
                "track",
                "momentary",
                json!({"kind": "mixer-channel", "track_id": TRACK_ID, "parameter": "solo"}),
            ),
            trigger_case(
                "reset-input",
                "rising",
                "scene",
                "toggle",
                json!({"kind": "mixer-send", "send_id": SEND_ID, "parameter": "enabled"}),
            ),
            trigger_case(
                "reset-input",
                "falling",
                "mixer-page",
                "momentary",
                json!({"kind": "mixer-main", "parameter": "mute"}),
            ),
            trigger_case(
                "clock-input",
                "rising",
                "global",
                "toggle",
                json!({"kind": "mixer-cue", "parameter": "mute"}),
            ),
            trigger_case(
                "clock-input",
                "falling",
                "track",
                "invoke",
                json!({"kind": "scene", "scene_id": SCENE_ID, "action": "launch"}),
            ),
            trigger_case(
                "reset-input",
                "rising",
                "scene",
                "invoke",
                json!({"kind": "transport", "action": "play"}),
            ),
            trigger_case(
                "reset-input",
                "falling",
                "global",
                "invoke",
                json!({"kind": "transport", "action": "stop"}),
            ),
            trigger_case(
                "clock-input",
                "rising",
                "mixer-page",
                "invoke",
                json!({"kind": "transport", "action": "toggle"}),
            ),
            trigger_case(
                "reset-input",
                "falling",
                "track",
                "invoke",
                json!({"kind": "transport", "action": "reset"}),
            ),
        ];

        for (source, scope, transform, target) in cases {
            let document = parse_mapping(&mapping_value(&source, &scope, &transform, &target));
            assert_eq!(
                document.extensions().get("org.example.fixture"),
                Some(&json!({"preserve": true}))
            );
        }
    }

    #[test]
    fn rejects_published_parser_boundaries() {
        let valid: Value = serde_json::from_slice(VALID).unwrap();
        let mut invalid_cases = Vec::new();

        let mut unknown_field = valid.clone();
        unknown_field["bindings"][0]["source"]["unexpected"] = true.into();
        invalid_cases.push(unknown_field);

        let mut cv_index = valid.clone();
        cv_index["bindings"][0]["source"]["input_index"] = 4.into();
        invalid_cases.push(cv_index);

        let mut input_bound = valid.clone();
        input_bound["bindings"][0]["transform"]["input_min"] = (-1_000_001).into();
        invalid_cases.push(input_bound);

        let mut empty_input_range = valid.clone();
        empty_input_range["bindings"][0]["transform"]["input_min"] = 1_000_000.into();
        invalid_cases.push(empty_input_range);

        let mut output_bound = valid.clone();
        output_bound["bindings"][0]["transform"]["output_max"] = 1_000_001.into();
        invalid_cases.push(output_bound);

        let mut invalid_id = valid.clone();
        invalid_id["bindings"][0]["id"] = "invalid".into();
        invalid_cases.push(invalid_id);

        let mut invalid_extension = valid.clone();
        invalid_extension["extensions"]["invalid"] = true.into();
        invalid_cases.push(invalid_extension);

        let mut unsorted = valid.clone();
        let first = unsorted["bindings"][0].clone();
        let mut second = first.clone();
        second["id"] = SECOND_BINDING_ID.into();
        unsorted["bindings"] = json!([second, first]);
        invalid_cases.push(unsorted);

        let mut duplicate_source_scope = valid;
        let mut second = duplicate_source_scope["bindings"][0].clone();
        second["id"] = SECOND_BINDING_ID.into();
        duplicate_source_scope["bindings"] =
            json!([duplicate_source_scope["bindings"][0].clone(), second]);
        invalid_cases.push(duplicate_source_scope);

        for value in invalid_cases {
            assert!(ControlMappingDocument::parse(&serde_json::to_vec(&value).unwrap()).is_err());
        }

        assert!(ControlMappingDocument::parse(&vec![b' '; MAX_DOCUMENT_BYTES + 1]).is_err());

        let binding = ControlMappingDocument::parse(VALID).unwrap().bindings[0].clone();
        let oversized = RawControlMappingDocument {
            format: FORMAT.to_owned(),
            format_version: FORMAT_VERSION,
            id: MAPPING_ID.to_owned(),
            project_id: PROJECT_ID.to_owned(),
            bindings: vec![binding; MAX_BINDINGS + 1],
            extensions: BTreeMap::new(),
        };
        assert!(matches!(
            validate_document(oversized),
            Err(error) if error.kind() == ControlMappingParseErrorKind::InvalidDomain
        ));
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one classified matrix keeps every public binding error and its expected identity visible together"
    )]
    fn classifies_every_project_binding_failure_and_preserves_binding_identity() {
        let continuous = || {
            parse_mapping(&mapping_value(
                &json!({"kind": "cv-input", "input_index": 0}),
                &json!({"kind": "global"}),
                &linear_transform(),
                &json!({"kind": "mixer-channel", "track_id": TRACK_ID, "parameter": "gain"}),
            ))
        };

        assert_binding_failure(
            &continuous(),
            FOREIGN_ID,
            &TestProject::default(),
            ControlMappingBindingErrorKind::DescriptorIdMismatch,
            None,
        );
        let foreign_project = TestProject::default().with_project_id(FOREIGN_ID);
        assert_binding_failure(
            &continuous(),
            MAPPING_ID,
            &foreign_project,
            ControlMappingBindingErrorKind::ProjectIdMismatch,
            None,
        );
        let document_collision = TestProject::default().with_nested_entity(MAPPING_ID);
        assert_binding_failure(
            &continuous(),
            MAPPING_ID,
            &document_collision,
            ControlMappingBindingErrorKind::DocumentIdentityCollision,
            None,
        );

        let mut cases = Vec::new();
        let project = TestProject::default().with_entity(BINDING_ID);
        cases.push((
            continuous(),
            project,
            ControlMappingBindingErrorKind::IdentityCollision,
        ));

        let project = TestProject::default().with_cv_inputs(0);
        cases.push((
            continuous(),
            project,
            ControlMappingBindingErrorKind::CvInputUnavailable {
                index: 0,
                available: 0,
            },
        ));

        let clock = edge_mapping(
            "clock-input",
            "rising",
            &json!({"kind": "global"}),
            &json!({"kind": "transport", "action": "play"}),
            "invoke",
        );
        let project = TestProject::default().without(ProjectFact::Clock);
        cases.push((
            clock,
            project,
            ControlMappingBindingErrorKind::ClockInputUnavailable,
        ));

        let reset = edge_mapping(
            "reset-input",
            "falling",
            &json!({"kind": "global"}),
            &json!({"kind": "transport", "action": "reset"}),
            "invoke",
        );
        let project = TestProject::default().without(ProjectFact::Reset);
        cases.push((
            reset,
            project,
            ControlMappingBindingErrorKind::ResetInputUnavailable,
        ));

        let track_scope = parse_mapping(&mapping_value(
            &json!({"kind": "cv-input", "input_index": 0}),
            &json!({"kind": "track", "track_id": TRACK_ID}),
            &linear_transform(),
            &json!({"kind": "mixer-main", "parameter": "gain"}),
        ));
        let project = TestProject::default().without(ProjectFact::Track);
        cases.push((
            track_scope,
            project,
            ControlMappingBindingErrorKind::TrackScopeMissing,
        ));

        let scene_scope = edge_mapping(
            "clock-input",
            "rising",
            &json!({"kind": "scene", "scene_id": SCENE_ID}),
            &json!({"kind": "transport", "action": "play"}),
            "invoke",
        );
        let project = TestProject::default().without(ProjectFact::Scene);
        cases.push((
            scene_scope,
            project,
            ControlMappingBindingErrorKind::SceneScopeMissing,
        ));

        let mixer_scope = edge_mapping(
            "clock-input",
            "rising",
            &json!({"kind": "mixer-page"}),
            &json!({"kind": "transport", "action": "play"}),
            "invoke",
        );
        let project = TestProject::default().without(ProjectFact::Mixer);
        cases.push((
            mixer_scope,
            project,
            ControlMappingBindingErrorKind::MixerScopeMissing,
        ));

        let project = TestProject::default().without(ProjectFact::MixerChannel);
        cases.push((
            continuous(),
            project,
            ControlMappingBindingErrorKind::MixerChannelMissing,
        ));

        let send = parse_mapping(&mapping_value(
            &json!({"kind": "cv-input", "input_index": 0}),
            &json!({"kind": "global"}),
            &linear_transform(),
            &json!({"kind": "mixer-send", "send_id": SEND_ID, "parameter": "gain"}),
        ));
        let project = TestProject::default().without(ProjectFact::MixerSend);
        cases.push((
            send,
            project,
            ControlMappingBindingErrorKind::MixerSendMissing,
        ));

        let scene_target = edge_mapping(
            "clock-input",
            "rising",
            &json!({"kind": "global"}),
            &json!({"kind": "scene", "scene_id": SCENE_ID, "action": "launch"}),
            "invoke",
        );
        let project = TestProject::default().without(ProjectFact::Scene);
        cases.push((
            scene_target,
            project,
            ControlMappingBindingErrorKind::SceneTargetMissing,
        ));

        let solo = edge_mapping(
            "clock-input",
            "rising",
            &json!({"kind": "global"}),
            &json!({"kind": "mixer-channel", "track_id": TRACK_ID, "parameter": "solo"}),
            "toggle",
        );
        let project = TestProject::default().without(ProjectFact::SoloSource);
        cases.push((
            solo,
            project,
            ControlMappingBindingErrorKind::SoloTargetNotSource,
        ));

        for target in [
            json!({"kind": "mixer-main", "parameter": "gain"}),
            json!({"kind": "mixer-cue", "parameter": "gain"}),
        ] {
            let document = parse_mapping(&mapping_value(
                &json!({"kind": "cv-input", "input_index": 0}),
                &json!({"kind": "global"}),
                &linear_transform(),
                &target,
            ));
            let project = TestProject::default().without(ProjectFact::Mixer);
            cases.push((
                document,
                project,
                ControlMappingBindingErrorKind::MixerTargetMissing,
            ));
        }

        for (document, project, expected) in cases {
            assert_binding_failure(&document, MAPPING_ID, &project, expected, Some(BINDING_ID));
        }

        for document in [
            edge_mapping(
                "clock-input",
                "rising",
                &json!({"kind": "mixer-page"}),
                &json!({"kind": "transport", "action": "play"}),
                "invoke",
            ),
            parse_mapping(&mapping_value(
                &json!({"kind": "cv-input", "input_index": 0}),
                &json!({"kind": "global"}),
                &linear_transform(),
                &json!({"kind": "mixer-main", "parameter": "gain"}),
            )),
            parse_mapping(&mapping_value(
                &json!({"kind": "cv-input", "input_index": 0}),
                &json!({"kind": "global"}),
                &linear_transform(),
                &json!({"kind": "mixer-cue", "parameter": "gain"}),
            )),
        ] {
            document
                .validate_for_project(MAPPING_ID, &TestProject::default())
                .unwrap();
        }
    }

    fn trigger_case(
        source_kind: &str,
        edge: &str,
        scope_kind: &str,
        behavior: &str,
        target: Value,
    ) -> (Value, Value, Value, Value) {
        let scope = match scope_kind {
            "track" => json!({"kind": "track", "track_id": TRACK_ID}),
            "scene" => json!({"kind": "scene", "scene_id": SCENE_ID}),
            kind => json!({"kind": kind}),
        };
        (
            json!({"kind": source_kind, "edge": edge}),
            scope,
            json!({"kind": "trigger", "behavior": behavior}),
            target,
        )
    }

    fn linear_transform() -> Value {
        json!({
            "kind": "linear",
            "input_min": -1_000_000,
            "input_max": 1_000_000,
            "output_min": 0,
            "output_max": 1_000_000
        })
    }

    fn continuous_binding(transform: ControlTransform, target: ControlTarget) -> ControlBinding {
        ControlBinding {
            id: BINDING_ID.to_owned(),
            enabled: true,
            source: ControlSource::CvInput { input_index: 0 },
            scope: ControlScope::Global,
            transform,
            target,
        }
    }

    fn edge_mapping(
        source_kind: &str,
        edge: &str,
        scope: &Value,
        target: &Value,
        behavior: &str,
    ) -> ControlMappingDocument {
        parse_mapping(&mapping_value(
            &json!({"kind": source_kind, "edge": edge}),
            scope,
            &json!({"kind": "trigger", "behavior": behavior}),
            target,
        ))
    }

    fn mapping_value(source: &Value, scope: &Value, transform: &Value, target: &Value) -> Value {
        json!({
            "format": FORMAT,
            "format_version": FORMAT_VERSION,
            "id": MAPPING_ID,
            "project_id": PROJECT_ID,
            "bindings": [{
                "id": BINDING_ID,
                "enabled": true,
                "source": source,
                "scope": scope,
                "transform": transform,
                "target": target
            }],
            "extensions": {"org.example.fixture": {"preserve": true}}
        })
    }

    fn parse_mapping(value: &Value) -> ControlMappingDocument {
        ControlMappingDocument::parse(&serde_json::to_vec(&value).unwrap()).unwrap()
    }

    fn assert_binding_failure(
        document: &ControlMappingDocument,
        descriptor_id: &str,
        project: &TestProject,
        expected: ControlMappingBindingErrorKind,
        binding_id: Option<&str>,
    ) {
        let error = document
            .validate_for_project(descriptor_id, project)
            .unwrap_err();
        assert_eq!(error.kind(), expected);
        assert_eq!(error.binding_id(), binding_id);
    }
}
