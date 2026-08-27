#![doc = "Validated persistent authority for reversible rendered audio-track freezes."]
#![forbid(unsafe_code)]

use hyper_project_model::is_valid_entity_id;
use hyper_strict_json::StrictJsonError;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

const FORMAT: &str = "hyper-track-freeze-state";
const FORMAT_VERSION: u8 = 4;
const LEGACY_FORMAT_VERSION: u8 = 3;
const MAX_DOCUMENT_BYTES: usize = 1_048_576;

/// The current manifest-declared documents that make one freeze reversible.
///
/// Callers must provide canonical JSON document bytes and sample-asset
/// identities from the same verified package.  The optional device-chain
/// digest is the manifest digest for the chain retained by the freeze.
#[derive(Clone, Copy, Debug)]
pub struct ActiveTrackFreezeAuthority<'a> {
    project_id: &'a str,
    track_document: &'a [u8],
    frozen_clip_document: &'a [u8],
    arrangement_document: &'a [u8],
    sample_asset_ids: &'a [&'a str],
    device_chain_sha256: Option<&'a str>,
}

impl<'a> ActiveTrackFreezeAuthority<'a> {
    #[must_use]
    pub const fn new(
        project_id: &'a str,
        track_document: &'a [u8],
        frozen_clip_document: &'a [u8],
        arrangement_document: &'a [u8],
        sample_asset_ids: &'a [&'a str],
        device_chain_sha256: Option<&'a str>,
    ) -> Self {
        Self {
            project_id,
            track_document,
            frozen_clip_document,
            arrangement_document,
            sample_asset_ids,
            device_chain_sha256,
        }
    }
}

/// The exact manifest identity retained for one bypassed native-device chain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrozenDeviceChainAuthority {
    id: String,
    sha256: String,
}

impl FrozenDeviceChainAuthority {
    /// Creates one validated device-chain identity and canonical SHA-256
    /// digest pair.
    ///
    /// # Errors
    ///
    /// Returns a domain rejection if either value cannot identify one
    /// manifest-declared device-chain resource.
    pub fn try_new(id: String, sha256: String) -> Result<Self, TrackFreezeStateError> {
        if !is_valid_entity_id(&id) || !is_canonical_sha256(&sha256) {
            return Err(TrackFreezeStateError::InvalidDomain(
                "track freeze state has an invalid device-chain authority",
            ));
        }
        Ok(Self { id, sha256 })
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

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
    project_id: String,
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
    #[allow(
        clippy::too_many_lines,
        reason = "the persistence boundary validates the complete retained clip before exposing reversible authority"
    )]
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
        let project_id = object
            .get("project_id")
            .and_then(serde_json::Value::as_str)
            .filter(|project_id| is_valid_entity_id(project_id))
            .ok_or(TrackFreezeStateError::InvalidDomain(
                "track freeze state has an invalid displaced clip document",
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
        let source = object
            .get("source")
            .and_then(serde_json::Value::as_object)
            .ok_or(TrackFreezeStateError::InvalidDomain(
                "track freeze state has an invalid displaced clip document",
            ))?;
        let loop_spec = object
            .get("loop")
            .and_then(serde_json::Value::as_object)
            .ok_or(TrackFreezeStateError::InvalidDomain(
                "track freeze state has an invalid displaced clip document",
            ))?;
        let playback = object
            .get("playback")
            .and_then(serde_json::Value::as_object)
            .ok_or(TrackFreezeStateError::InvalidDomain(
                "track freeze state has an invalid displaced clip document",
            ))?;
        let fades = object
            .get("fades")
            .and_then(serde_json::Value::as_object)
            .ok_or(TrackFreezeStateError::InvalidDomain(
                "track freeze state has an invalid displaced clip document",
            ))?;
        if object.get("format").and_then(serde_json::Value::as_str) != Some("hyper-audio-clip")
            || !matches!(
                object
                    .get("format_version")
                    .and_then(serde_json::Value::as_u64),
                Some(1 | 2)
            )
            || object
                .get("name")
                .and_then(serde_json::Value::as_str)
                .is_none()
            || object
                .get("enabled")
                .and_then(serde_json::Value::as_bool)
                .is_none()
            || object
                .get("duration_ticks")
                .and_then(serde_json::Value::as_u64)
                .is_none()
            || source
                .get("start_frame")
                .and_then(serde_json::Value::as_u64)
                .is_none()
            || source
                .get("frame_count")
                .and_then(serde_json::Value::as_u64)
                .is_none()
            || loop_spec
                .get("enabled")
                .and_then(serde_json::Value::as_bool)
                .is_none()
            || loop_spec
                .get("start_tick")
                .and_then(serde_json::Value::as_u64)
                .is_none()
            || loop_spec
                .get("end_tick")
                .and_then(serde_json::Value::as_u64)
                .is_none()
            || playback
                .get("gain_decibels")
                .and_then(serde_json::Value::as_f64)
                .is_none()
            || playback
                .get("reverse")
                .and_then(serde_json::Value::as_bool)
                .is_none()
            || playback
                .get("transpose_cents")
                .and_then(serde_json::Value::as_i64)
                .is_none()
            || fades.get("curve").and_then(serde_json::Value::as_str) != Some("linear-amplitude")
            || fades
                .get("in_ticks")
                .and_then(serde_json::Value::as_u64)
                .is_none()
            || fades
                .get("out_ticks")
                .and_then(serde_json::Value::as_u64)
                .is_none()
            || object
                .get("warp_markers")
                .and_then(serde_json::Value::as_array)
                .is_none()
            || object
                .get("extensions")
                .and_then(serde_json::Value::as_object)
                .is_none()
        {
            return Err(TrackFreezeStateError::InvalidDomain(
                "track freeze state has an incomplete displaced clip document",
            ));
        }
        Ok(Self {
            id,
            project_id,
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
    pub fn project_id(&self) -> &str {
        &self.project_id
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
    format_version: u8,
    id: String,
    project_id: String,
    track_id: String,
    frozen_clip_id: String,
    frozen_asset_id: String,
    frozen_clip: FrozenClipState,
    arrangement_id: String,
    frozen_placement_id: String,
    frozen_placement: Option<FrozenPlacementState>,
    original_clips: Vec<FrozenClipState>,
    original_placements: Vec<FrozenPlacementState>,
    device_chain_id: Option<String>,
    device_chain_sha256: Option<String>,
    extensions: BTreeMap<String, serde_json::Value>,
}

impl TrackFreezeState {
    /// Creates one validated version-4 freeze authority.
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
        frozen_clip: FrozenClipState,
        arrangement_id: String,
        frozen_placement_id: String,
        frozen_placement: FrozenPlacementState,
        original_clips: Vec<FrozenClipState>,
        original_placements: Vec<FrozenPlacementState>,
        device_chain: Option<FrozenDeviceChainAuthority>,
    ) -> Result<Self, TrackFreezeStateError> {
        validate(RawTrackFreezeState {
            format: FORMAT.to_owned(),
            format_version: FORMAT_VERSION,
            id,
            project_id,
            track_id,
            frozen_clip_id,
            frozen_asset_id,
            frozen_clip: RawFrozenClipState {
                document: frozen_clip.document,
            },
            arrangement_id,
            frozen_placement_id,
            frozen_placement: Some(RawFrozenPlacementState {
                document: frozen_placement.document,
            }),
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
            device_chain_id: device_chain.as_ref().map(|authority| authority.id.clone()),
            device_chain_sha256: device_chain.map(|authority| authority.sha256),
            extensions: BTreeMap::new(),
        })
    }

    /// Parses one complete version-3 freeze-state resource.
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
    pub const fn format_version(&self) -> u8 {
        self.format_version
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
    pub const fn frozen_clip(&self) -> &FrozenClipState {
        &self.frozen_clip
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
    pub fn frozen_placement(&self) -> Option<&FrozenPlacementState> {
        self.frozen_placement.as_ref()
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
    pub fn device_chain_sha256(&self) -> Option<&str> {
        self.device_chain_sha256.as_deref()
    }
    /// Returns whether this state records every current rendered authority
    /// needed to keep a frozen track immutable before unfreeze.
    #[must_use]
    pub const fn has_complete_rendered_authority(&self) -> bool {
        self.format_version == FORMAT_VERSION
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
            format_version: self.format_version,
            id: &self.id,
            project_id: &self.project_id,
            track_id: &self.track_id,
            frozen_clip_id: &self.frozen_clip_id,
            frozen_asset_id: &self.frozen_asset_id,
            frozen_clip: &self.frozen_clip,
            arrangement_id: &self.arrangement_id,
            frozen_placement_id: &self.frozen_placement_id,
            frozen_placement: self.frozen_placement.as_ref(),
            original_clips: &self.original_clips,
            original_placements: &self.original_placements,
            device_chain_id: self.device_chain_id.as_deref(),
            device_chain_sha256: self.device_chain_sha256.as_deref(),
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
    frozen_clip: RawFrozenClipState,
    arrangement_id: String,
    frozen_placement_id: String,
    #[serde(default)]
    frozen_placement: Option<RawFrozenPlacementState>,
    original_clips: Vec<RawFrozenClipState>,
    original_placements: Vec<RawFrozenPlacementState>,
    #[serde(default)]
    device_chain_id: Option<String>,
    #[serde(default)]
    device_chain_sha256: Option<String>,
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
    frozen_clip: &'a FrozenClipState,
    arrangement_id: &'a str,
    frozen_placement_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    frozen_placement: Option<&'a FrozenPlacementState>,
    original_clips: &'a [FrozenClipState],
    original_placements: &'a [FrozenPlacementState],
    #[serde(skip_serializing_if = "Option::is_none")]
    device_chain_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    device_chain_sha256: Option<&'a str>,
    extensions: &'a BTreeMap<String, serde_json::Value>,
}

#[allow(
    clippy::too_many_lines,
    reason = "the one persistence boundary validates every cross-resource identity before exposing reversible authority"
)]
fn validate(raw: RawTrackFreezeState) -> Result<TrackFreezeState, TrackFreezeStateError> {
    if raw.format != FORMAT || !matches!(raw.format_version, LEGACY_FORMAT_VERSION | FORMAT_VERSION)
    {
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
    let device_chain_is_valid = match (
        raw.device_chain_id.as_deref(),
        raw.device_chain_sha256.as_deref(),
    ) {
        (None, None) => true,
        (Some(id), Some(sha256)) => {
            is_valid_entity_id(id) && is_canonical_sha256(sha256) && unique.insert(id.to_owned())
        }
        (Some(id), None) if raw.format_version == LEGACY_FORMAT_VERSION => {
            is_valid_entity_id(id) && unique.insert(id.to_owned())
        }
        _ => false,
    };
    if !device_chain_is_valid {
        return Err(TrackFreezeStateError::InvalidDomain(
            "track freeze state has an invalid device-chain authority",
        ));
    }
    let frozen_clip = FrozenClipState::try_new(raw.frozen_clip.document)?;
    if frozen_clip.id() != raw.frozen_clip_id
        || frozen_clip.project_id() != raw.project_id
        || frozen_clip.track_id() != raw.track_id
        || frozen_clip.asset_id() != raw.frozen_asset_id
    {
        return Err(TrackFreezeStateError::InvalidDomain(
            "track freeze state has an invalid rendered clip",
        ));
    }
    let frozen_placement = match (raw.format_version, raw.frozen_placement) {
        (FORMAT_VERSION, Some(raw_placement)) => {
            let placement = FrozenPlacementState::try_new(raw_placement.document)?;
            let frozen_duration = frozen_clip
                .document()
                .get("duration_ticks")
                .and_then(serde_json::Value::as_u64);
            if placement.id() != raw.frozen_placement_id
                || placement
                    .document()
                    .get("track_id")
                    .and_then(serde_json::Value::as_str)
                    != Some(raw.track_id.as_str())
                || placement
                    .document()
                    .get("clip_id")
                    .and_then(serde_json::Value::as_str)
                    != Some(raw.frozen_clip_id.as_str())
                || placement
                    .document()
                    .get("start_tick")
                    .and_then(serde_json::Value::as_u64)
                    != Some(0)
                || placement
                    .document()
                    .get("clip_start_tick")
                    .and_then(serde_json::Value::as_u64)
                    != Some(0)
                || placement
                    .document()
                    .get("duration_ticks")
                    .and_then(serde_json::Value::as_u64)
                    != frozen_duration
                || placement
                    .document()
                    .get("enabled")
                    .and_then(serde_json::Value::as_bool)
                    != Some(true)
            {
                return Err(TrackFreezeStateError::InvalidDomain(
                    "track freeze state has an invalid rendered placement",
                ));
            }
            Some(placement)
        }
        (LEGACY_FORMAT_VERSION, None) => None,
        _ => {
            return Err(TrackFreezeStateError::InvalidDomain(
                "track freeze state has an invalid rendered placement authority",
            ));
        }
    };
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
        if clip.project_id() != raw.project_id.as_str()
            || clip.track_id() != raw.track_id.as_str()
            || !unique.insert(clip.id().to_owned())
        {
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
        format_version: raw.format_version,
        id: raw.id,
        project_id: raw.project_id,
        track_id: raw.track_id,
        frozen_clip_id: raw.frozen_clip_id,
        frozen_asset_id: raw.frozen_asset_id,
        frozen_clip,
        arrangement_id: raw.arrangement_id,
        frozen_placement_id: raw.frozen_placement_id,
        frozen_placement,
        original_clips,
        original_placements,
        device_chain_id: raw.device_chain_id,
        device_chain_sha256: raw.device_chain_sha256,
        extensions: raw.extensions,
    })
}

/// Verifies that the active project graph still represents exactly one
/// reversible rendered freeze.
///
/// The caller owns package-level role, descriptor-identity, and duplicate-state
/// checks.  This function owns the freeze record's current track, rendered
/// clip, rendered placement, retained sample, and bypassed device-chain
/// authority.
///
/// # Errors
///
/// Returns a domain rejection if an active document is noncanonical or if any
/// retained freeze authority differs from the current project graph.
#[allow(
    clippy::too_many_lines,
    reason = "the reversible freeze contract compares the complete current track, rendered clip, placement, assets, and device-chain authority together"
)]
pub fn validate_active_track_freeze_authority(
    state: &TrackFreezeState,
    authority: ActiveTrackFreezeAuthority<'_>,
) -> Result<(), TrackFreezeStateError> {
    if state.project_id() != authority.project_id {
        return Err(TrackFreezeStateError::InvalidDomain(
            "track freeze state project authority is inconsistent",
        ));
    }
    let track = parse_canonical_active_document(
        authority.track_document,
        "active frozen track is not canonical JSON",
    )?;
    let arrangement = parse_canonical_active_document(
        authority.arrangement_document,
        "active frozen arrangement is not canonical JSON",
    )?;
    let retained_frozen_clip = serde_json_canonicalizer::to_vec(state.frozen_clip().document())
        .map_err(|_| {
            TrackFreezeStateError::InvalidDomain(
                "track freeze state rendered clip cannot be canonicalized",
            )
        })?;
    if retained_frozen_clip != authority.frozen_clip_document {
        return Err(TrackFreezeStateError::InvalidDomain(
            "track freeze state rendered clip authority differs from the active clip",
        ));
    }
    if track.get("id").and_then(serde_json::Value::as_str) != Some(state.track_id())
        || track.get("project_id").and_then(serde_json::Value::as_str) != Some(state.project_id())
        || track.get("kind").and_then(serde_json::Value::as_str) != Some("audio")
        || track
            .get("clip_ids")
            .and_then(serde_json::Value::as_array)
            .is_none_or(|ids| ids.len() != 1 || ids[0].as_str() != Some(state.frozen_clip_id()))
    {
        return Err(TrackFreezeStateError::InvalidDomain(
            "active frozen track no longer contains exactly its rendered clip",
        ));
    }
    if arrangement.get("id").and_then(serde_json::Value::as_str) != Some(state.arrangement_id())
        || arrangement
            .get("project_id")
            .and_then(serde_json::Value::as_str)
            != Some(state.project_id())
    {
        return Err(TrackFreezeStateError::InvalidDomain(
            "active frozen arrangement identity is inconsistent",
        ));
    }
    let target = arrangement
        .get("placements")
        .and_then(serde_json::Value::as_array)
        .ok_or(TrackFreezeStateError::InvalidDomain(
            "active frozen arrangement has no placement collection",
        ))?
        .iter()
        .filter(|placement| {
            placement
                .get("track_id")
                .and_then(serde_json::Value::as_str)
                == Some(state.track_id())
        })
        .collect::<Vec<_>>();
    let frozen_duration = state
        .frozen_clip()
        .document()
        .get("duration_ticks")
        .and_then(serde_json::Value::as_u64);
    if target.len() != 1
        || target[0].get("id").and_then(serde_json::Value::as_str)
            != Some(state.frozen_placement_id())
        || target[0].get("clip_id").and_then(serde_json::Value::as_str)
            != Some(state.frozen_clip_id())
        || target[0]
            .get("start_tick")
            .and_then(serde_json::Value::as_u64)
            != Some(0)
        || target[0]
            .get("clip_start_tick")
            .and_then(serde_json::Value::as_u64)
            != Some(0)
        || target[0]
            .get("duration_ticks")
            .and_then(serde_json::Value::as_u64)
            != frozen_duration
        || target[0]
            .get("enabled")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
        || state
            .frozen_placement()
            .is_some_and(|retained| target[0] != retained.document())
    {
        return Err(TrackFreezeStateError::InvalidDomain(
            "active frozen arrangement no longer contains its rendered placement",
        ));
    }
    if !authority
        .sample_asset_ids
        .iter()
        .any(|id| *id == state.frozen_asset_id())
        || state.original_clips().iter().any(|clip| {
            !authority
                .sample_asset_ids
                .iter()
                .any(|id| *id == clip.asset_id())
        })
    {
        return Err(TrackFreezeStateError::InvalidDomain(
            "track freeze state retains a missing sample asset",
        ));
    }
    match (
        state.device_chain_id(),
        state.device_chain_sha256(),
        authority.device_chain_sha256,
    ) {
        (None, None, None) => {}
        (Some(_), Some(expected), Some(active)) if expected == active => {}
        (Some(_), None, Some(_)) if state.format_version() == LEGACY_FORMAT_VERSION => {}
        _ => {
            return Err(TrackFreezeStateError::InvalidDomain(
                "track freeze state device-chain authority differs from the active project",
            ));
        }
    }
    Ok(())
}

fn parse_canonical_active_document(
    bytes: &[u8],
    message: &'static str,
) -> Result<serde_json::Value, TrackFreezeStateError> {
    let document: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| TrackFreezeStateError::InvalidDomain(message))?;
    let canonical = serde_json_canonicalizer::to_vec(&document)
        .map_err(|_| TrackFreezeStateError::InvalidDomain(message))?;
    if canonical != bytes {
        return Err(TrackFreezeStateError::InvalidDomain(message));
    }
    Ok(document)
}

fn is_canonical_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::{TrackFreezeState, TrackFreezeStateError};
    use serde_json::Value;

    const STATE: &[u8] = br#"{"format":"hyper-track-freeze-state","format_version":2,"id":"018f47a2-6b2d-7c11-8a44-001122337600","project_id":"018f47a2-6b2d-7c11-8a44-001122337601","track_id":"018f47a2-6b2d-7c11-8a44-001122337602","frozen_clip_id":"018f47a2-6b2d-7c11-8a44-001122337603","frozen_asset_id":"018f47a2-6b2d-7c11-8a44-001122337604","arrangement_id":"018f47a2-6b2d-7c11-8a44-001122337605","frozen_placement_id":"018f47a2-6b2d-7c11-8a44-001122337606","original_clips":[{"document":{"format":"hyper-audio-clip","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122337607","project_id":"018f47a2-6b2d-7c11-8a44-001122337601","track_id":"018f47a2-6b2d-7c11-8a44-001122337602","name":"Lead","enabled":true,"duration_ticks":4,"source":{"asset_id":"018f47a2-6b2d-7c11-8a44-001122337610","start_frame":0,"frame_count":100},"loop":{"enabled":false,"start_tick":0,"end_tick":4},"playback":{"gain_decibels":0,"reverse":false,"transpose_cents":0},"fades":{"curve":"linear-amplitude","in_ticks":0,"out_ticks":0},"warp_markers":[],"extensions":{}}}],"original_placements":[{"document":{"id":"018f47a2-6b2d-7c11-8a44-001122337608","track_id":"018f47a2-6b2d-7c11-8a44-001122337602","clip_id":"018f47a2-6b2d-7c11-8a44-001122337607","start_tick":0,"clip_start_tick":0,"duration_ticks":4,"enabled":true}}],"device_chain_id":"018f47a2-6b2d-7c11-8a44-001122337609","extensions":{}}"#;

    fn state_bytes() -> Vec<u8> {
        let mut state: Value = serde_json::from_slice(STATE).unwrap();
        state["format_version"] = 4.into();
        let mut frozen = state["original_clips"][0]["document"].clone();
        frozen["id"] = "018f47a2-6b2d-7c11-8a44-001122337603".into();
        frozen["source"]["asset_id"] = "018f47a2-6b2d-7c11-8a44-001122337604".into();
        state["frozen_clip"] = serde_json::json!({"document": frozen});
        state["frozen_placement"] = serde_json::json!({"document": {
            "id":"018f47a2-6b2d-7c11-8a44-001122337606",
            "track_id":"018f47a2-6b2d-7c11-8a44-001122337602",
            "clip_id":"018f47a2-6b2d-7c11-8a44-001122337603",
            "start_tick":0,
            "clip_start_tick":0,
            "duration_ticks":4,
            "enabled":true
        }});
        state["device_chain_sha256"] = Value::String(
            "0000000000000000000000000000000000000000000000000000000000000000".into(),
        );
        serde_json_canonicalizer::to_vec(&state).unwrap()
    }

    fn legacy_state_bytes() -> Vec<u8> {
        let mut state: Value = serde_json::from_slice(&state_bytes()).unwrap();
        state["format_version"] = 3.into();
        state.as_object_mut().unwrap().remove("frozen_placement");
        state.as_object_mut().unwrap().remove("device_chain_sha256");
        serde_json_canonicalizer::to_vec(&state).unwrap()
    }

    #[test]
    fn parses_complete_reversible_authority() {
        let state = TrackFreezeState::parse(&state_bytes()).unwrap();
        assert_eq!(state.track_id(), "018f47a2-6b2d-7c11-8a44-001122337602");
        assert_eq!(state.original_clips().len(), 1);
        assert_eq!(
            state.original_placements()[0].document()["duration_ticks"],
            4
        );
        assert!(state.device_chain_id().is_some());
        assert!(state.frozen_placement().is_some());
        assert!(state.has_complete_rendered_authority());
        let round_trip = TrackFreezeState::parse(&state.to_canonical_bytes().unwrap()).unwrap();
        assert_eq!(state, round_trip);
    }

    #[test]
    fn retains_legacy_freeze_states_only_for_safe_reading_and_unfreeze() {
        let state = TrackFreezeState::parse(&legacy_state_bytes()).unwrap();
        assert_eq!(state.format_version(), 3);
        assert!(state.frozen_placement().is_none());
        assert!(!state.has_complete_rendered_authority());
        let round_trip = TrackFreezeState::parse(&state.to_canonical_bytes().unwrap()).unwrap();
        assert_eq!(state, round_trip);
    }

    #[test]
    fn refuses_a_duplicate_original_clip_identity() {
        let mut duplicated: Value = serde_json::from_slice(&state_bytes()).unwrap();
        let clips = duplicated["original_clips"].as_array_mut().unwrap();
        clips.push(clips[0].clone());
        let duplicated = serde_json_canonicalizer::to_vec(&duplicated).unwrap();
        assert!(matches!(
            TrackFreezeState::parse(&duplicated),
            Err(TrackFreezeStateError::InvalidDomain(_))
        ));
    }

    #[test]
    fn refuses_the_nonreversible_version_one_schema() {
        let state = String::from_utf8(state_bytes()).unwrap();
        let legacy = state.replace("\"format_version\":4", "\"format_version\":1");
        assert!(matches!(
            TrackFreezeState::parse(legacy.as_bytes()),
            Err(TrackFreezeStateError::InvalidDomain(_))
        ));
    }
}
