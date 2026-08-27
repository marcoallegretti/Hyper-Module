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

/// One complete arrangement placement displaced by a rendered freeze clip.
///
/// The original document is retained rather than a reduced projection: an
/// unfreeze must restore the exact placement, including its clip range and
/// any versioned extension data.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FrozenPlacementState {
    #[serde(skip_serializing)]
    id: String,
    document: serde_json::Value,
}

impl FrozenPlacementState {
    /// Creates one validated displaced-placement record.
    ///
    /// # Errors
    ///
    /// Returns a domain rejection when the placement cannot be restored as a
    /// complete arrangement placement.
    pub fn try_new(document: serde_json::Value) -> Result<Self, TrackFreezeStateError> {
        let object = document
            .as_object()
            .ok_or(TrackFreezeStateError::InvalidDomain(
                "track freeze state displaced placement is not an object",
            ))?;
        let id = object
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| is_valid_entity_id(id))
            .ok_or(TrackFreezeStateError::InvalidDomain(
                "track freeze state has an invalid displaced placement identity",
            ))?
            .to_owned();
        if object
            .get("track_id")
            .and_then(serde_json::Value::as_str)
            .is_none_or(|track_id| !is_valid_entity_id(track_id))
            || object
                .get("clip_id")
                .and_then(serde_json::Value::as_str)
                .is_none_or(|clip_id| !is_valid_entity_id(clip_id))
            || object
                .get("start_tick")
                .and_then(serde_json::Value::as_u64)
                .is_none()
            || object
                .get("clip_start_tick")
                .and_then(serde_json::Value::as_u64)
                .is_none()
            || object
                .get("duration_ticks")
                .and_then(serde_json::Value::as_u64)
                .is_none()
            || object
                .get("enabled")
                .and_then(serde_json::Value::as_bool)
                .is_none()
        {
            return Err(TrackFreezeStateError::InvalidDomain(
                "track freeze state has an invalid displaced placement document",
            ));
        }
        Ok(Self { id, document })
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub const fn document(&self) -> &serde_json::Value {
        &self.document
    }
}

/// One complete audio-clip document displaced by a rendered freeze clip.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FrozenClipState {
    #[serde(skip_serializing)]
    id: String,
    #[serde(skip_serializing)]
    track_id: String,
    #[serde(skip_serializing)]
    asset_id: String,
    document: serde_json::Value,
}

impl FrozenClipState {
    /// Creates one validated displaced-clip record.
    ///
    /// # Errors
    ///
    /// Returns a domain rejection when the clip cannot be restored as a
    /// complete audio-clip document.
    pub fn try_new(document: serde_json::Value) -> Result<Self, TrackFreezeStateError> {
        let object = document
            .as_object()
            .ok_or(TrackFreezeStateError::InvalidDomain(
                "track freeze state displaced clip is not an object",
            ))?;
        let id = object
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| is_valid_entity_id(id))
            .ok_or(TrackFreezeStateError::InvalidDomain(
                "track freeze state has an invalid displaced clip identity",
            ))?
            .to_owned();
        let track_id = object
            .get("track_id")
            .and_then(serde_json::Value::as_str)
            .filter(|track_id| is_valid_entity_id(track_id))
            .ok_or(TrackFreezeStateError::InvalidDomain(
                "track freeze state has an invalid displaced clip document",
            ))?
            .to_owned();
        let asset_id = object
            .get("source")
            .and_then(serde_json::Value::as_object)
            .and_then(|source| source.get("asset_id"))
            .and_then(serde_json::Value::as_str)
            .filter(|asset_id| is_valid_entity_id(asset_id))
            .ok_or(TrackFreezeStateError::InvalidDomain(
                "track freeze state has an invalid displaced clip document",
            ))?
            .to_owned();
        Ok(Self {
            id,
            track_id,
            asset_id,
            document,
        })
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn track_id(&self) -> &str {
        &self.track_id
    }

    #[must_use]
    pub fn asset_id(&self) -> &str {
        &self.asset_id
    }

    #[must_use]
    pub const fn document(&self) -> &serde_json::Value {
        &self.document
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
    original_clips: Vec<FrozenClipState>,
    original_placements: Vec<FrozenPlacementState>,
    device_chain_id: Option<String>,
    extensions: BTreeMap<String, serde_json::Value>,
}

impl TrackFreezeState {
    /// Creates one validated version-1 freeze authority.
    ///
    /// # Errors
    ///
    /// Returns a domain rejection when identities or displaced source state
    /// cannot describe one reversible audio-track freeze.
    #[allow(
        clippy::too_many_arguments,
        reason = "the persisted authority intentionally names every resource that freeze and unfreeze must coordinate"
    )]
    pub fn try_new(
        id: String,
        project_id: String,
        track_id: String,
        frozen_clip_id: String,
        frozen_asset_id: String,
        arrangement_id: String,
        frozen_placement_id: String,
        original_clips: Vec<FrozenClipState>,
        original_placements: Vec<FrozenPlacementState>,
        device_chain_id: Option<String>,
    ) -> Result<Self, TrackFreezeStateError> {
        validate(RawTrackFreezeState {
            format: FORMAT.to_owned(),
            format_version: FORMAT_VERSION,
            id,
            project_id,
            track_id,
            frozen_clip_id,
            frozen_asset_id,
            arrangement_id,
            frozen_placement_id,
            original_clips: original_clips
                .into_iter()
                .map(|clip| RawFrozenClipState {
                    document: clip.document,
                })
                .collect(),
            original_placements: original_placements
                .into_iter()
                .map(|placement| RawFrozenPlacementState {
                    document: placement.document,
                })
                .collect(),
            device_chain_id,
            extensions: BTreeMap::new(),
        })
    }

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
    pub fn original_clips(&self) -> &[FrozenClipState] {
        &self.original_clips
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

    /// Returns canonical JSON suitable for one manifest-declared resource.
    ///
    /// # Errors
    ///
    /// Returns a serialization error if canonical JSON capacity is unavailable.
    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json_canonicalizer::to_vec(&CanonicalTrackFreezeState {
            format: FORMAT,
            format_version: FORMAT_VERSION,
            id: &self.id,
            project_id: &self.project_id,
            track_id: &self.track_id,
            frozen_clip_id: &self.frozen_clip_id,
            frozen_asset_id: &self.frozen_asset_id,
            arrangement_id: &self.arrangement_id,
            frozen_placement_id: &self.frozen_placement_id,
            original_clips: &self.original_clips,
            original_placements: &self.original_placements,
            device_chain_id: self.device_chain_id.as_deref(),
            extensions: &self.extensions,
        })
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
    original_clips: Vec<RawFrozenClipState>,
    original_placements: Vec<RawFrozenPlacementState>,
    #[serde(default)]
    device_chain_id: Option<String>,
    extensions: BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFrozenPlacementState {
    document: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFrozenClipState {
    document: serde_json::Value,
}

#[derive(Serialize)]
struct CanonicalTrackFreezeState<'a> {
    format: &'static str,
    format_version: u8,
    id: &'a str,
    project_id: &'a str,
    track_id: &'a str,
    frozen_clip_id: &'a str,
    frozen_asset_id: &'a str,
    arrangement_id: &'a str,
    frozen_placement_id: &'a str,
    original_clips: &'a [FrozenClipState],
    original_placements: &'a [FrozenPlacementState],
    #[serde(skip_serializing_if = "Option::is_none")]
    device_chain_id: Option<&'a str>,
    extensions: &'a BTreeMap<String, serde_json::Value>,
}

#[allow(
    clippy::too_many_lines,
    reason = "the one persistence boundary validates every cross-resource identity before exposing reversible authority"
)]
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
        .any(|value| !unique.insert((*value).clone()))
    {
        return Err(TrackFreezeStateError::InvalidDomain(
            "track freeze state identities must be distinct",
        ));
    }
    if let Some(device_chain_id) = &raw.device_chain_id
        && (!is_valid_entity_id(device_chain_id) || !unique.insert(device_chain_id.clone()))
    {
        return Err(TrackFreezeStateError::InvalidDomain(
            "track freeze state has an invalid device-chain identity",
        ));
    }
    if raw.original_clips.is_empty() {
        return Err(TrackFreezeStateError::InvalidDomain(
            "track freeze state has no displaced clips",
        ));
    }
    let mut original_clips = Vec::new();
    original_clips
        .try_reserve_exact(raw.original_clips.len())
        .map_err(|_| {
            TrackFreezeStateError::InvalidDomain("track freeze state clip capacity is unavailable")
        })?;
    for raw_clip in raw.original_clips {
        let clip = FrozenClipState::try_new(raw_clip.document)?;
        if clip.track_id() != raw.track_id.as_str() || !unique.insert(clip.id().to_owned()) {
            return Err(TrackFreezeStateError::InvalidDomain(
                "track freeze state has invalid displaced clips",
            ));
        }
        original_clips.push(clip);
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
    for raw_placement in raw.original_placements {
        let placement = FrozenPlacementState::try_new(raw_placement.document)?;
        if placement
            .document()
            .get("track_id")
            .and_then(serde_json::Value::as_str)
            != Some(raw.track_id.as_str())
            || placement
                .document()
                .get("clip_id")
                .and_then(serde_json::Value::as_str)
                .is_none_or(|clip_id| !original_clips.iter().any(|clip| clip.id() == clip_id))
            || placement.id == raw.frozen_placement_id
            || !placement_ids.insert(placement.id.clone())
        {
            return Err(TrackFreezeStateError::InvalidDomain(
                "track freeze state has invalid displaced placements",
            ));
        }
        original_placements.push(placement);
    }
    Ok(TrackFreezeState {
        id: raw.id,
        project_id: raw.project_id,
        track_id: raw.track_id,
        frozen_clip_id: raw.frozen_clip_id,
        frozen_asset_id: raw.frozen_asset_id,
        arrangement_id: raw.arrangement_id,
        frozen_placement_id: raw.frozen_placement_id,
        original_clips,
        original_placements,
        device_chain_id: raw.device_chain_id,
        extensions: raw.extensions,
    })
}

#[cfg(test)]
mod tests {
    use super::{TrackFreezeState, TrackFreezeStateError};

    const STATE: &[u8] = br#"{"format":"hyper-track-freeze-state","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122337600","project_id":"018f47a2-6b2d-7c11-8a44-001122337601","track_id":"018f47a2-6b2d-7c11-8a44-001122337602","frozen_clip_id":"018f47a2-6b2d-7c11-8a44-001122337603","frozen_asset_id":"018f47a2-6b2d-7c11-8a44-001122337604","arrangement_id":"018f47a2-6b2d-7c11-8a44-001122337605","frozen_placement_id":"018f47a2-6b2d-7c11-8a44-001122337606","original_clips":[{"document":{"id":"018f47a2-6b2d-7c11-8a44-001122337607","track_id":"018f47a2-6b2d-7c11-8a44-001122337602","source":{"asset_id":"018f47a2-6b2d-7c11-8a44-001122337610"}}}],"original_placements":[{"document":{"id":"018f47a2-6b2d-7c11-8a44-001122337608","track_id":"018f47a2-6b2d-7c11-8a44-001122337602","clip_id":"018f47a2-6b2d-7c11-8a44-001122337607","start_tick":0,"clip_start_tick":0,"duration_ticks":4,"enabled":true}}],"device_chain_id":"018f47a2-6b2d-7c11-8a44-001122337609","extensions":{}}"#;

    #[test]
    fn parses_complete_reversible_authority() {
        let state = TrackFreezeState::parse(STATE).unwrap();
        assert_eq!(state.track_id(), "018f47a2-6b2d-7c11-8a44-001122337602");
        assert_eq!(state.original_clips().len(), 1);
        assert_eq!(
            state.original_placements()[0].document()["duration_ticks"],
            4
        );
        assert!(state.device_chain_id().is_some());
        let round_trip = TrackFreezeState::parse(&state.to_canonical_bytes().unwrap()).unwrap();
        assert_eq!(state, round_trip);
    }

    #[test]
    fn refuses_a_duplicate_original_clip_identity() {
        let duplicated = br#"{"format":"hyper-track-freeze-state","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122337600","project_id":"018f47a2-6b2d-7c11-8a44-001122337601","track_id":"018f47a2-6b2d-7c11-8a44-001122337602","frozen_clip_id":"018f47a2-6b2d-7c11-8a44-001122337603","frozen_asset_id":"018f47a2-6b2d-7c11-8a44-001122337604","arrangement_id":"018f47a2-6b2d-7c11-8a44-001122337605","frozen_placement_id":"018f47a2-6b2d-7c11-8a44-001122337606","original_clips":[{"document":{"id":"018f47a2-6b2d-7c11-8a44-001122337607","track_id":"018f47a2-6b2d-7c11-8a44-001122337602","source":{"asset_id":"018f47a2-6b2d-7c11-8a44-001122337610"}}},{"document":{"id":"018f47a2-6b2d-7c11-8a44-001122337607","track_id":"018f47a2-6b2d-7c11-8a44-001122337602","source":{"asset_id":"018f47a2-6b2d-7c11-8a44-001122337610"}}}],"original_placements":[{"document":{"id":"018f47a2-6b2d-7c11-8a44-001122337608","track_id":"018f47a2-6b2d-7c11-8a44-001122337602","clip_id":"018f47a2-6b2d-7c11-8a44-001122337607","start_tick":0,"clip_start_tick":0,"duration_ticks":4,"enabled":true}}],"extensions":{}}"#;
        assert!(matches!(
            TrackFreezeState::parse(duplicated),
            Err(TrackFreezeStateError::InvalidDomain(_))
        ));
    }
}
