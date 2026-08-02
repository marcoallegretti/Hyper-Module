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
    IdentityCollision,
    CvInputUnavailable { index: u8, available: u8 },
    ClockInputUnavailable,
    ResetInputUnavailable,
    TrackScopeMissing,
    SceneScopeMissing,
    MixerChannelMissing,
    MixerSendMissing,
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
            ControlMappingBindingErrorKind::MixerChannelMissing => {
                formatter.write_str("target mixer channel does not exist")
            }
            ControlMappingBindingErrorKind::MixerSendMissing => {
                formatter.write_str("target mixer send does not exist")
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
        ControlScope::Global
        | ControlScope::MixerPage
        | ControlScope::Track { .. }
        | ControlScope::Scene { .. } => {}
    }
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

    const VALID: &[u8] = include_bytes!("../../../contracts/fixtures/valid/control-mappings.json");
    const INVALID_ACTION_LINEAR: &[u8] =
        include_bytes!("../../../contracts/fixtures/invalid/control-mappings-action-linear.json");

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
}
