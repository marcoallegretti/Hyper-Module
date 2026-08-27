#![doc = "Validated persistent authority for reversible rendered audio-track freezes."]
#![forbid(unsafe_code)]

use hyper_project_model::is_valid_entity_id;
use hyper_strict_json::StrictJsonError;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

const FORMAT: &str = "hyper-track-freeze-state";
const FORMAT_VERSION: u8 = 1;
const MAX_DOCUMENT_BYTES: usize = 1_048_576;

/// The preserved enabled state of one arrangement placement displaced by a
/// rendered freeze clip.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FrozenPlacementState {
    id: String,
    enabled: bool,
}

impl FrozenPlacementState {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }
}

/// Complete durable authority needed to unfreeze one rendered audio track.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrackFreezeState {
    id: String,
    project_id: String,
    track_id: String,
    frozen_clip_id: String,
    frozen_asset_id: String,
    arrangement_id: String,
    frozen_placement_id: String,
    original_clip_ids: Vec<String>,
    original_placements: Vec<FrozenPlacementState>,
    device_chain_id: Option<String>,
    extensions: BTreeMap<String, serde_json::Value>,
}

impl TrackFreezeState {
    /// Parses one complete version-1 freeze-state resource.
    ///
    /// # Errors
    ///
    /// Returns a strict JSON or freeze-domain rejection.
    pub fn parse(bytes: &[u8]) -> Result<Self, TrackFreezeStateError> {
        let raw: RawTrackFreezeState = hyper_strict_json::parse(bytes, MAX_DOCUMENT_BYTES)?;
        validate(raw)
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
    pub fn track_id(&self) -> &str {
        &self.track_id
    }
    #[must_use]
    pub fn frozen_clip_id(&self) -> &str {
        &self.frozen_clip_id
    }
    #[must_use]
    pub fn frozen_asset_id(&self) -> &str {
        &self.frozen_asset_id
    }
    #[must_use]
    pub fn arrangement_id(&self) -> &str {
        &self.arrangement_id
    }
    #[must_use]
    pub fn frozen_placement_id(&self) -> &str {
        &self.frozen_placement_id
    }
    #[must_use]
    pub fn original_clip_ids(&self) -> &[String] {
        &self.original_clip_ids
    }
    #[must_use]
    pub fn original_placements(&self) -> &[FrozenPlacementState] {
        &self.original_placements
    }
    #[must_use]
    pub fn device_chain_id(&self) -> Option<&str> {
        self.device_chain_id.as_deref()
    }
    #[must_use]
    pub const fn extensions(&self) -> &BTreeMap<String, serde_json::Value> {
        &self.extensions
    }
}

#[derive(Debug)]
pub enum TrackFreezeStateError {
    Json(StrictJsonError),
    InvalidDomain(&'static str),
}

impl fmt::Display for TrackFreezeStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(error) => error.fmt(formatter),
            Self::InvalidDomain(message) => formatter.write_str(message),
        }
    }
}

impl Error for TrackFreezeStateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Json(error) => Some(error),
            Self::InvalidDomain(_) => None,
        }
    }
}

impl From<StrictJsonError> for TrackFreezeStateError {
    fn from(error: StrictJsonError) -> Self {
        Self::Json(error)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTrackFreezeState {
    format: String,
    format_version: u8,
    id: String,
    project_id: String,
    track_id: String,
    frozen_clip_id: String,
    frozen_asset_id: String,
    arrangement_id: String,
    frozen_placement_id: String,
    original_clip_ids: Vec<String>,
    original_placements: Vec<RawFrozenPlacementState>,
    #[serde(default)]
    device_chain_id: Option<String>,
    extensions: BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFrozenPlacementState {
    id: String,
    enabled: bool,
}

fn validate(raw: RawTrackFreezeState) -> Result<TrackFreezeState, TrackFreezeStateError> {
    if raw.format != FORMAT || raw.format_version != FORMAT_VERSION {
        return Err(TrackFreezeStateError::InvalidDomain(
            "track freeze state format is unsupported",
        ));
    }
    let identities = [
        &raw.id,
        &raw.project_id,
        &raw.track_id,
        &raw.frozen_clip_id,
        &raw.frozen_asset_id,
        &raw.arrangement_id,
        &raw.frozen_placement_id,
    ];
    if identities.iter().any(|value| !is_valid_entity_id(value)) {
        return Err(TrackFreezeStateError::InvalidDomain(
            "track freeze state contains an invalid identity",
        ));
    }
    let mut unique = BTreeSet::new();
    if identities
        .iter()
        .any(|value| !unique.insert(value.as_str()))
    {
        return Err(TrackFreezeStateError::InvalidDomain(
            "track freeze state identities must be distinct",
        ));
    }
    if let Some(device_chain_id) = &raw.device_chain_id
        && (!is_valid_entity_id(device_chain_id) || !unique.insert(device_chain_id))
    {
        return Err(TrackFreezeStateError::InvalidDomain(
            "track freeze state has an invalid device-chain identity",
        ));
    }
    if raw.original_clip_ids.is_empty()
        || raw
            .original_clip_ids
            .iter()
            .any(|id| !is_valid_entity_id(id))
        || raw
            .original_clip_ids
            .iter()
            .any(|id| !unique.insert(id.as_str()))
    {
        return Err(TrackFreezeStateError::InvalidDomain(
            "track freeze state has invalid original clip identities",
        ));
    }
    if raw.original_placements.is_empty() {
        return Err(TrackFreezeStateError::InvalidDomain(
            "track freeze state has no displaced placements",
        ));
    }
    let mut placement_ids = BTreeSet::new();
    let mut original_placements = Vec::new();
    original_placements
        .try_reserve_exact(raw.original_placements.len())
        .map_err(|_| {
            TrackFreezeStateError::InvalidDomain(
                "track freeze state placement capacity is unavailable",
            )
        })?;
    for placement in raw.original_placements {
        if !is_valid_entity_id(&placement.id)
            || placement.id == raw.frozen_placement_id
            || !placement_ids.insert(placement.id.clone())
        {
            return Err(TrackFreezeStateError::InvalidDomain(
                "track freeze state has invalid displaced placements",
            ));
        }
        original_placements.push(FrozenPlacementState {
            id: placement.id,
            enabled: placement.enabled,
        });
    }
    Ok(TrackFreezeState {
        id: raw.id,
        project_id: raw.project_id,
        track_id: raw.track_id,
        frozen_clip_id: raw.frozen_clip_id,
        frozen_asset_id: raw.frozen_asset_id,
        arrangement_id: raw.arrangement_id,
        frozen_placement_id: raw.frozen_placement_id,
        original_clip_ids: raw.original_clip_ids,
        original_placements,
        device_chain_id: raw.device_chain_id,
        extensions: raw.extensions,
    })
}

#[cfg(test)]
mod tests {
    use super::{TrackFreezeState, TrackFreezeStateError};

    const STATE: &[u8] = br#"{"format":"hyper-track-freeze-state","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122337600","project_id":"018f47a2-6b2d-7c11-8a44-001122337601","track_id":"018f47a2-6b2d-7c11-8a44-001122337602","frozen_clip_id":"018f47a2-6b2d-7c11-8a44-001122337603","frozen_asset_id":"018f47a2-6b2d-7c11-8a44-001122337604","arrangement_id":"018f47a2-6b2d-7c11-8a44-001122337605","frozen_placement_id":"018f47a2-6b2d-7c11-8a44-001122337606","original_clip_ids":["018f47a2-6b2d-7c11-8a44-001122337607"],"original_placements":[{"id":"018f47a2-6b2d-7c11-8a44-001122337608","enabled":true}],"device_chain_id":"018f47a2-6b2d-7c11-8a44-001122337609","extensions":{}}"#;

    #[test]
    fn parses_complete_reversible_authority() {
        let state = TrackFreezeState::parse(STATE).unwrap();
        assert_eq!(state.track_id(), "018f47a2-6b2d-7c11-8a44-001122337602");
        assert_eq!(state.original_clip_ids().len(), 1);
        assert!(state.original_placements()[0].enabled());
        assert!(state.device_chain_id().is_some());
    }

    #[test]
    fn refuses_a_duplicate_original_clip_identity() {
        let duplicated = br#"{"format":"hyper-track-freeze-state","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122337600","project_id":"018f47a2-6b2d-7c11-8a44-001122337601","track_id":"018f47a2-6b2d-7c11-8a44-001122337602","frozen_clip_id":"018f47a2-6b2d-7c11-8a44-001122337603","frozen_asset_id":"018f47a2-6b2d-7c11-8a44-001122337604","arrangement_id":"018f47a2-6b2d-7c11-8a44-001122337605","frozen_placement_id":"018f47a2-6b2d-7c11-8a44-001122337606","original_clip_ids":["018f47a2-6b2d-7c11-8a44-001122337607","018f47a2-6b2d-7c11-8a44-001122337607"],"original_placements":[{"id":"018f47a2-6b2d-7c11-8a44-001122337608","enabled":true}],"extensions":{}}"#;
        assert!(matches!(
            TrackFreezeState::parse(duplicated),
            Err(TrackFreezeStateError::InvalidDomain(_))
        ));
    }
}
