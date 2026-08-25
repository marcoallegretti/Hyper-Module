use core::error::Error;
use core::fmt;
use std::collections::{BTreeSet, HashMap};

use hyper_native_device::{DeviceChain, DeviceChainError};
use hyper_project_model::is_valid_entity_id;
use serde_json::Value;

use crate::{
    AdapterCompileError, AutomationCompileError, DocumentKind, DocumentLocation, GraphLimits,
    LoadError, LoadedPlaybackProject, LoaderLimits, MediaAssetFact,
    MixerCompileError, SceneLimits, capacity_error, load_arrangement, load_clip, load_scene,
    load_track, validate_loaded_identities, validate_track_hierarchy,
};

/// One canonical resource document emitted by independent track duplication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrackDuplicationDocument {
    id: String,
    bytes: Vec<u8>,
}

impl TrackDuplicationDocument {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Complete canonical resource set for one independent track copy.
///
/// Every `Some` or collection member replaces or creates exactly the resource
/// identified by its enclosing document. Callers must publish this set as one
/// transaction; publishing a subset would make its identity graph incomplete.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrackDuplicationDocuments {
    track: TrackDuplicationDocument,
    clips: Vec<TrackDuplicationDocument>,
    mixer: TrackDuplicationDocument,
    arrangement: Option<TrackDuplicationDocument>,
    scenes: Vec<TrackDuplicationDocument>,
    automation_takes: Vec<TrackDuplicationDocument>,
    device_chain: Option<TrackDuplicationDocument>,
}

impl TrackDuplicationDocuments {
    #[must_use]
    pub const fn track(&self) -> &TrackDuplicationDocument {
        &self.track
    }

    #[must_use]
    pub fn clips(&self) -> &[TrackDuplicationDocument] {
        &self.clips
    }

    #[must_use]
    pub const fn mixer(&self) -> &TrackDuplicationDocument {
        &self.mixer
    }

    #[must_use]
    pub const fn arrangement(&self) -> Option<&TrackDuplicationDocument> {
        self.arrangement.as_ref()
    }

    #[must_use]
    pub fn scenes(&self) -> &[TrackDuplicationDocument] {
        &self.scenes
    }

    #[must_use]
    pub fn automation_takes(&self) -> &[TrackDuplicationDocument] {
        &self.automation_takes
    }

    #[must_use]
    pub const fn device_chain(&self) -> Option<&TrackDuplicationDocument> {
        self.device_chain.as_ref()
    }
}

/// Backward-compatible name for the audio-track specialization.
pub type AudioTrackDuplicationDocument = TrackDuplicationDocument;

/// Backward-compatible name for the audio-track specialization.
pub type AudioTrackDuplicationDocuments = TrackDuplicationDocuments;

/// Complete verified source authority for audio-track duplication.
///
/// The caller supplies every current automation document because automation is
/// stored outside the playback document graph. Track, clip, scene, arrangement
/// and mixer bytes are independently rechecked against `project` before any
/// output is returned. `device_chain` is the source track's single verified
/// native chain, if one exists.
#[derive(Clone, Copy, Debug)]
pub struct AudioTrackDuplicationSource<'a> {
    pub track: &'a [u8],
    pub clips: &'a [(&'a str, &'a [u8])],
    pub scenes: &'a [(&'a str, &'a [u8])],
    pub arrangement: Option<&'a [u8]>,
    pub mixer: &'a [u8],
    /// Complete manifest-declared automation-take identity inventory.
    pub automation_take_ids: &'a [&'a str],
    pub automation_takes: &'a [(&'a str, &'a [u8])],
    /// The manifest-declared source-track chain identity, if one exists.
    pub device_chain_id: Option<&'a str>,
    pub device_chain: Option<&'a [u8]>,
}

/// Rejection returned before an incomplete audio-track copy can be published.
#[derive(Debug)]
pub enum AudioTrackDuplicationSerializationError {
    TrackMissing,
    TrackNotAudio,
    ParentTrackUnsupported,
    TrackCapacity,
    ClipCapacity,
    InvalidTrackName,
    InvalidGeneratedIdentity,
    SourceTrackMismatch,
    SourceClipCoverage,
    SourceSceneCoverage,
    SourceArrangementMismatch,
    SourceMixerMismatch,
    SourceAutomationCoverage,
    SourceDeviceChainMismatch,
    SceneSlotCapacity,
    InvalidDocumentShape,
    UnknownExtensionSemantics,
    Source(LoadError),
    Device(DeviceChainError),
    Serialization(serde_json::Error),
    ProjectCompilation(AdapterCompileError),
    MixerCompilation(MixerCompileError),
    AutomationCompilation(AutomationCompileError),
}

impl fmt::Display for AudioTrackDuplicationSerializationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TrackMissing => formatter.write_str("the selected track is not present"),
            Self::TrackNotAudio => formatter.write_str("track duplication supports audio tracks only"),
            Self::ParentTrackUnsupported => {
                formatter.write_str("track duplication does not infer hierarchy copy semantics")
            }
            Self::TrackCapacity => formatter.write_str("the audio-track capacity is exhausted"),
            Self::ClipCapacity => formatter.write_str("the audio-clip capacity is exhausted"),
            Self::InvalidTrackName => {
                formatter.write_str("the duplicated track name is outside the version-1 domain")
            }
            Self::InvalidGeneratedIdentity => {
                formatter.write_str("a generated duplication identity is invalid or collides")
            }
            Self::SourceTrackMismatch => {
                formatter.write_str("source track bytes do not match the loaded project")
            }
            Self::SourceClipCoverage => {
                formatter.write_str("source clip documents do not exactly cover the selected track")
            }
            Self::SourceSceneCoverage => {
                formatter.write_str("source scene documents do not exactly cover the project")
            }
            Self::SourceArrangementMismatch => {
                formatter.write_str("source arrangement bytes do not match the loaded project")
            }
            Self::SourceMixerMismatch => {
                formatter.write_str("source mixer bytes do not match the loaded project")
            }
            Self::SourceAutomationCoverage => {
                formatter.write_str("source automation documents have duplicate or misaddressed identities")
            }
            Self::SourceDeviceChainMismatch => {
                formatter.write_str("source native device chain does not belong to the selected track")
            }
            Self::SceneSlotCapacity => formatter.write_str("a scene has no capacity for the duplicated action"),
            Self::InvalidDocumentShape => {
                formatter.write_str("a source document shape changed after typed validation")
            }
            Self::UnknownExtensionSemantics => formatter.write_str(
                "track duplication refuses a transformed document with unknown extension identity semantics",
            ),
            Self::Source(source) => source.fmt(formatter),
            Self::Device(source) => source.fmt(formatter),
            Self::Serialization(source) => source.fmt(formatter),
            Self::ProjectCompilation(source) => source.fmt(formatter),
            Self::MixerCompilation(source) => source.fmt(formatter),
            Self::AutomationCompilation(source) => source.fmt(formatter),
        }
    }
}

impl Error for AudioTrackDuplicationSerializationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Source(source) => Some(source),
            Self::Device(source) => Some(source),
            Self::Serialization(source) => Some(source),
            Self::ProjectCompilation(source) => Some(source),
            Self::MixerCompilation(source) => Some(source),
            Self::AutomationCompilation(source) => Some(source),
            Self::TrackMissing
            | Self::TrackNotAudio
            | Self::ParentTrackUnsupported
            | Self::TrackCapacity
            | Self::ClipCapacity
            | Self::InvalidTrackName
            | Self::InvalidGeneratedIdentity
            | Self::SourceTrackMismatch
            | Self::SourceClipCoverage
            | Self::SourceSceneCoverage
            | Self::SourceArrangementMismatch
            | Self::SourceMixerMismatch
            | Self::SourceAutomationCoverage
            | Self::SourceDeviceChainMismatch
            | Self::SceneSlotCapacity
            | Self::InvalidDocumentShape
            | Self::UnknownExtensionSemantics => None,
        }
    }
}

/// Builds one complete independent audio-track copy from verified current
/// resources. The identity provider is invoked only after source authority and
/// capacity have been checked; every returned value is revalidated against the
/// complete candidate identity domain before it can appear in output.
///
/// `source.automation_takes` must be the complete current automation inventory
/// in manifest order. Automation is stored separately from playback documents,
/// so a caller that cannot prove that inventory must refuse before calling this
/// serializer rather than silently leave a source-targeted lane unpaired.
///
/// # Errors
///
/// Returns a classified refusal for stale or noncanonical source authority,
/// unsupported source topology, exhausted capacity, unknown extension identity
/// semantics, invalid generated identities, or a candidate graph that cannot
/// load and compile as one complete project.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "track duplication deliberately validates and rewrites one complete mutable identity graph at one boundary"
)]
pub fn serialize_audio_track_duplication(
    project: &LoadedPlaybackProject,
    assets: &[MediaAssetFact<'_>],
    source: AudioTrackDuplicationSource<'_>,
    source_track_id: &str,
    duplicated_track_name: &str,
    limits: LoaderLimits,
    mut next_identity: impl FnMut() -> String,
) -> Result<AudioTrackDuplicationDocuments, AudioTrackDuplicationSerializationError> {
    if project.tracks.len() >= limits.max_track_documents {
        return Err(AudioTrackDuplicationSerializationError::TrackCapacity);
    }
    let name_length = duplicated_track_name.chars().count();
    if duplicated_track_name.trim() != duplicated_track_name
        || !(1..=super::MAX_VERSION_1_NAME_CHARACTERS).contains(&name_length)
    {
        return Err(AudioTrackDuplicationSerializationError::InvalidTrackName);
    }

    let source_track_index = project
        .track_index(source_track_id)
        .ok_or(AudioTrackDuplicationSerializationError::TrackMissing)?;
    let loaded_source_track = load_track(
        source.track,
        source_track_index,
        project.project_id(),
        limits.max_document_bytes,
        limits.max_track_clip_references,
    )
    .map_err(AudioTrackDuplicationSerializationError::Source)?;
    let expected_source_track = &project.tracks[source_track_index];
    if !same_track(&loaded_source_track, expected_source_track) {
        return Err(AudioTrackDuplicationSerializationError::TrackMissing);
    }
    if loaded_source_track.kind != super::TrackKind::Audio {
        return Err(AudioTrackDuplicationSerializationError::TrackNotAudio);
    }
    if loaded_source_track.parent_track_id.is_some() {
        return Err(AudioTrackDuplicationSerializationError::ParentTrackUnsupported);
    }
    let next_order = (0..=7)
        .find(|order| project.tracks.iter().all(|track| track.order != *order))
        .ok_or(AudioTrackDuplicationSerializationError::TrackCapacity)?;

    if project
        .clips
        .len()
        .checked_add(loaded_source_track.clip_ids.len())
        .is_none_or(|count| count > limits.max_clip_documents)
    {
        return Err(AudioTrackDuplicationSerializationError::ClipCapacity);
    }
    let (mut source_track_value, canonical_track) = canonical_value(source.track)?;
    if canonical_track != source.track {
        return Err(AudioTrackDuplicationSerializationError::SourceTrackMismatch);
    }
    require_empty_extensions(&source_track_value)?;
    let source_clips = source_clip_map(project, &loaded_source_track, source.clips, limits)?;
    let source_scenes = source_scene_map(project, source.scenes, limits)?;
    let source_arrangement = source_arrangement(project, source.arrangement, limits)?;

    let (mixer_value, canonical_mixer) = canonical_value(source.mixer)?;
    if canonical_mixer != source.mixer {
        return Err(AudioTrackDuplicationSerializationError::SourceMixerMismatch);
    }
    require_empty_extensions(&mixer_value)?;
    let loaded_mixer = project
        .load_mixer(source.mixer, assets, limits)
        .map_err(AudioTrackDuplicationSerializationError::Source)?;

    let mut expected_automation_ids = BTreeSet::new();
    for id in source.automation_take_ids {
        if !is_valid_entity_id(id) || !expected_automation_ids.insert(*id) {
            return Err(AudioTrackDuplicationSerializationError::SourceAutomationCoverage);
        }
    }
    if expected_automation_ids.len() != source.automation_takes.len() {
        return Err(AudioTrackDuplicationSerializationError::SourceAutomationCoverage);
    }
    let mut automation_bytes = Vec::new();
    automation_bytes
        .try_reserve_exact(source.automation_takes.len())
        .map_err(|_| AudioTrackDuplicationSerializationError::Source(capacity_error()))?;
    let mut automation_ids = BTreeSet::new();
    for (id, bytes) in source.automation_takes {
        if !automation_ids.insert(*id) || !expected_automation_ids.contains(*id) {
            return Err(AudioTrackDuplicationSerializationError::SourceAutomationCoverage);
        }
        let (_, canonical) = canonical_value(bytes)?;
        if canonical != *bytes {
            return Err(AudioTrackDuplicationSerializationError::SourceAutomationCoverage);
        }
        automation_bytes.push(*bytes);
    }
    let source_automation = project
        .load_automation_takes(&loaded_mixer, assets, &automation_bytes, limits)
        .map_err(AudioTrackDuplicationSerializationError::Source)?;
    if source_automation.takes().len() != source.automation_takes.len()
        || source_automation
            .takes()
            .iter()
            .zip(source.automation_takes)
            .any(|(take, (id, _))| take.id() != *id)
    {
        return Err(AudioTrackDuplicationSerializationError::SourceAutomationCoverage);
    }

    let mut known_identities = known_identities(project, assets, &loaded_mixer, &source_automation);
    let parsed_chain = match (source.device_chain_id, source.device_chain) {
        (None, None) => None,
        (Some(_), None) | (None, Some(_)) => {
            return Err(AudioTrackDuplicationSerializationError::SourceDeviceChainMismatch);
        }
        (Some(expected_id), Some(bytes)) => Some({
            let (_, canonical) = canonical_value(bytes)?;
            if canonical != bytes {
                return Err(AudioTrackDuplicationSerializationError::SourceDeviceChainMismatch);
            }
            let chain = DeviceChain::parse(bytes).map_err(AudioTrackDuplicationSerializationError::Device)?;
            if chain.id() != expected_id
                || chain.project_id() != project.project_id()
                || chain.track_id() != source_track_id
            {
                return Err(AudioTrackDuplicationSerializationError::SourceDeviceChainMismatch);
            }
            if !known_identities.insert(chain.id().to_owned())
                || chain
                    .devices()
                    .iter()
                    .any(|device| !known_identities.insert(device.id().to_owned()))
            {
                return Err(AudioTrackDuplicationSerializationError::SourceDeviceChainMismatch);
            }
            Ok(chain)
        }?),
    };

    let duplicated_track_id = allocate_identity(&mut known_identities, &mut next_identity)?;
    let mut cloned_clip_ids = HashMap::new();
    cloned_clip_ids
        .try_reserve(loaded_source_track.clip_ids.len())
        .map_err(|_| AudioTrackDuplicationSerializationError::Source(capacity_error()))?;
    let mut clips = Vec::new();
    clips
        .try_reserve_exact(loaded_source_track.clip_ids.len())
        .map_err(|_| AudioTrackDuplicationSerializationError::Source(capacity_error()))?;
    let mut candidate = project.clone();
    for source_clip_id in &loaded_source_track.clip_ids {
        let source_bytes = source_clips
            .get(source_clip_id.as_str())
            .copied()
            .ok_or(AudioTrackDuplicationSerializationError::SourceClipCoverage)?;
        let (mut value, _) = canonical_value(source_bytes)?;
        require_empty_extensions(&value)?;
        let duplicated_clip_id = allocate_identity(&mut known_identities, &mut next_identity)?;
        rewrite_string(&mut value, "id", &duplicated_clip_id)?;
        rewrite_string(&mut value, "track_id", &duplicated_track_id)?;
        let bytes = canonical_bytes(&value)?;
        let loaded = load_clip(
            &bytes,
            project.clips.len() + clips.len(),
            project.project_id(),
            limits.max_document_bytes,
            limits.max_warp_markers,
        )
        .map_err(AudioTrackDuplicationSerializationError::Source)?;
        cloned_clip_ids.insert(source_clip_id.as_str(), duplicated_clip_id.clone());
        clips.push(AudioTrackDuplicationDocument {
            id: duplicated_clip_id,
            bytes,
        });
        candidate
            .clips
            .try_reserve(1)
            .map_err(|_| AudioTrackDuplicationSerializationError::Source(capacity_error()))?;
        candidate.clips.push(loaded);
    }

    rewrite_string(&mut source_track_value, "id", &duplicated_track_id)?;
    rewrite_string(&mut source_track_value, "name", duplicated_track_name)?;
    replace_u8(&mut source_track_value, "order", next_order)?;
    replace_string_array(
        &mut source_track_value,
        "clip_ids",
        loaded_source_track
            .clip_ids
            .iter()
            .map(|id| {
                cloned_clip_ids
                    .get(id.as_str())
                    .cloned()
                    .ok_or(AudioTrackDuplicationSerializationError::SourceClipCoverage)
            })
            .collect::<Result<Vec<_>, _>>()?,
    )?;
    let track_bytes = canonical_bytes(&source_track_value)?;
    let cloned_track = load_track(
        &track_bytes,
        project.tracks.len(),
        project.project_id(),
        limits.max_document_bytes,
        limits.max_track_clip_references,
    )
    .map_err(AudioTrackDuplicationSerializationError::Source)?;
    candidate
        .tracks
        .try_reserve(1)
        .map_err(|_| AudioTrackDuplicationSerializationError::Source(capacity_error()))?;
    candidate.tracks.push(cloned_track);
    validate_track_hierarchy(&candidate.tracks).map_err(AudioTrackDuplicationSerializationError::Source)?;

    let arrangement = duplicate_arrangement(
        &mut candidate,
        project,
        source_arrangement,
        source_track_id,
        &duplicated_track_id,
        &cloned_clip_ids,
        &mut known_identities,
        &mut next_identity,
        limits,
    )?;
    let scenes = duplicate_scene_actions(
        &mut candidate,
        project,
        &source_scenes,
        source_track_id,
        &duplicated_track_id,
        &cloned_clip_ids,
        limits,
    )?;
    validate_loaded_identities(&candidate).map_err(AudioTrackDuplicationSerializationError::Source)?;

    let (mixer, source_send_ids, candidate_mixer) = duplicate_mixer(
        &candidate,
        source.mixer,
        &loaded_mixer,
        source_track_id,
        &duplicated_track_id,
        assets,
        limits,
        &mut known_identities,
        &mut next_identity,
    )?;
    let automation_takes = duplicate_automation_lanes(
        &candidate,
        assets,
        source.automation_takes,
        source_track_id,
        &duplicated_track_id,
        &source_send_ids,
        &candidate_mixer,
        limits,
        &mut known_identities,
        &mut next_identity,
    )?;
    candidate
        .compile_playback(assets, GraphLimits::VERSION_1, SceneLimits::VERSION_1)
        .map_err(AudioTrackDuplicationSerializationError::ProjectCompilation)?;

    let device_chain = parsed_chain
        .map(|chain| {
            let chain_id = allocate_identity(&mut known_identities, &mut next_identity)?;
            let mut device_ids = Vec::new();
            device_ids
                .try_reserve_exact(chain.devices().len())
                .map_err(|_| AudioTrackDuplicationSerializationError::Source(capacity_error()))?;
            for _ in chain.devices() {
                device_ids.push(allocate_identity(&mut known_identities, &mut next_identity)?);
            }
            let cloned = chain
                .try_clone_for_track(chain_id.clone(), duplicated_track_id.clone(), &device_ids)
                .map_err(AudioTrackDuplicationSerializationError::Device)?;
            let bytes = cloned
                .to_canonical_vec()
                .map_err(AudioTrackDuplicationSerializationError::Device)?;
            Ok(AudioTrackDuplicationDocument { id: chain_id, bytes })
        })
        .transpose()?;

    Ok(AudioTrackDuplicationDocuments {
        track: AudioTrackDuplicationDocument {
            id: duplicated_track_id,
            bytes: track_bytes,
        },
        clips,
        mixer,
        arrangement,
        scenes,
        automation_takes,
        device_chain,
    })
}

fn source_clip_map<'a>(
    project: &LoadedPlaybackProject,
    source_track: &super::LoadedTrack,
    source: &'a [(&'a str, &'a [u8])],
    limits: LoaderLimits,
) -> Result<HashMap<&'a str, &'a [u8]>, AudioTrackDuplicationSerializationError> {
    if source.len() != source_track.clip_ids.len() {
        return Err(AudioTrackDuplicationSerializationError::SourceClipCoverage);
    }
    let mut output = HashMap::new();
    output
        .try_reserve(source.len())
        .map_err(|_| AudioTrackDuplicationSerializationError::Source(capacity_error()))?;
    for (id, bytes) in source {
        let clip_index = project
            .clips
            .iter()
            .position(|clip| clip.id == *id)
            .ok_or(AudioTrackDuplicationSerializationError::SourceClipCoverage)?;
        let loaded = load_clip(
            bytes,
            clip_index,
            project.project_id(),
            limits.max_document_bytes,
            limits.max_warp_markers,
        )
        .map_err(AudioTrackDuplicationSerializationError::Source)?;
        if loaded.id != *id
            || loaded.track_id != source_track.id
            || loaded != project.clips[clip_index]
            || output.insert(*id, *bytes).is_some()
        {
            return Err(AudioTrackDuplicationSerializationError::SourceClipCoverage);
        }
        let (_, canonical) = canonical_value(bytes)?;
        if canonical != *bytes {
            return Err(AudioTrackDuplicationSerializationError::SourceClipCoverage);
        }
    }
    if source_track
        .clip_ids
        .iter()
        .any(|id| !output.contains_key(id.as_str()))
    {
        return Err(AudioTrackDuplicationSerializationError::SourceClipCoverage);
    }
    Ok(output)
}

fn source_scene_map<'a>(
    project: &LoadedPlaybackProject,
    source: &'a [(&'a str, &'a [u8])],
    limits: LoaderLimits,
) -> Result<HashMap<&'a str, &'a [u8]>, AudioTrackDuplicationSerializationError> {
    if source.len() != project.scenes.len() {
        return Err(AudioTrackDuplicationSerializationError::SourceSceneCoverage);
    }
    let mut output = HashMap::new();
    output
        .try_reserve(source.len())
        .map_err(|_| AudioTrackDuplicationSerializationError::Source(capacity_error()))?;
    for (id, bytes) in source {
        let scene_index = project
            .scenes
            .iter()
            .position(|scene| scene.id == *id)
            .ok_or(AudioTrackDuplicationSerializationError::SourceSceneCoverage)?;
        let loaded = load_scene(
            bytes,
            scene_index,
            project.project_id(),
            limits.max_document_bytes,
            limits.max_scene_slots,
        )
        .map_err(AudioTrackDuplicationSerializationError::Source)?;
        if loaded.id != *id
            || loaded != project.scenes[scene_index]
            || output.insert(*id, *bytes).is_some()
        {
            return Err(AudioTrackDuplicationSerializationError::SourceSceneCoverage);
        }
        let (_, canonical) = canonical_value(bytes)?;
        if canonical != *bytes {
            return Err(AudioTrackDuplicationSerializationError::SourceSceneCoverage);
        }
    }
    Ok(output)
}

fn source_arrangement<'a>(
    project: &LoadedPlaybackProject,
    bytes: Option<&'a [u8]>,
    limits: LoaderLimits,
) -> Result<Option<&'a [u8]>, AudioTrackDuplicationSerializationError> {
    match (&project.arrangement, bytes) {
        (None, None) => Ok(None),
        (Some(_), None) | (None, Some(_)) => {
            Err(AudioTrackDuplicationSerializationError::SourceArrangementMismatch)
        }
        (Some(expected), Some(bytes)) => {
            let loaded = load_arrangement(
                bytes,
                project.project_id(),
                limits.max_document_bytes,
                limits.max_placements,
                limits.max_scene_launches,
            )
            .map_err(AudioTrackDuplicationSerializationError::Source)?;
            let (_, canonical) = canonical_value(bytes)?;
            if loaded != *expected || canonical != bytes {
                return Err(AudioTrackDuplicationSerializationError::SourceArrangementMismatch);
            }
            Ok(Some(bytes))
        }
    }
}

fn known_identities(
    project: &LoadedPlaybackProject,
    assets: &[MediaAssetFact<'_>],
    mixer: &super::LoadedMixer,
    automation: &super::LoadedAutomationSet,
) -> BTreeSet<String> {
    let mut output = BTreeSet::new();
    let _ = output.insert(project.project_id.clone());
    let _ = output.insert(project.timeline_id.clone());
    for track in &project.tracks {
        let _ = output.insert(track.id.clone());
    }
    for clip in &project.clips {
        let _ = output.insert(clip.id.clone());
    }
    for scene in &project.scenes {
        let _ = output.insert(scene.id.clone());
    }
    if let Some(arrangement) = &project.arrangement {
        let _ = output.insert(arrangement.id.clone());
        for placement in &arrangement.placements {
            let _ = output.insert(placement.id.clone());
        }
        for launch in &arrangement.scene_launches {
            let _ = output.insert(launch.id.clone());
        }
    }
    let _ = output.insert(mixer.id.clone());
    for channel in &mixer.channels {
        for send in &channel.sends {
            let _ = output.insert(send.id.clone());
        }
    }
    for asset in assets {
        let _ = output.insert(asset.id.to_owned());
    }
    for take in automation.takes() {
        let _ = output.insert(take.id().to_owned());
        for lane in take.lanes() {
            let _ = output.insert(lane.id().to_owned());
        }
    }
    output
}

fn allocate_identity(
    known: &mut BTreeSet<String>,
    next_identity: &mut impl FnMut() -> String,
) -> Result<String, AudioTrackDuplicationSerializationError> {
    let identity = next_identity();
    if !is_valid_entity_id(&identity) || !known.insert(identity.clone()) {
        return Err(AudioTrackDuplicationSerializationError::InvalidGeneratedIdentity);
    }
    Ok(identity)
}

fn same_track(left: &super::LoadedTrack, right: &super::LoadedTrack) -> bool {
    left.id == right.id
        && left.name == right.name
        && left.kind == right.kind
        && left.order == right.order
        && left.active == right.active
        && left.parent_track_id == right.parent_track_id
        && left.clip_ids == right.clip_ids
}

fn canonical_value(
    bytes: &[u8],
) -> Result<(Value, Vec<u8>), AudioTrackDuplicationSerializationError> {
    let value = serde_json::from_slice(bytes).map_err(AudioTrackDuplicationSerializationError::Serialization)?;
    let canonical = canonical_bytes(&value)?;
    Ok((value, canonical))
}

fn canonical_bytes(value: &Value) -> Result<Vec<u8>, AudioTrackDuplicationSerializationError> {
    serde_json_canonicalizer::to_vec(value)
        .map_err(AudioTrackDuplicationSerializationError::Serialization)
}

fn require_empty_extensions(value: &Value) -> Result<(), AudioTrackDuplicationSerializationError> {
    if extensions_are_empty(value)? {
        Ok(())
    } else {
        Err(AudioTrackDuplicationSerializationError::UnknownExtensionSemantics)
    }
}

fn extensions_are_empty(value: &Value) -> Result<bool, AudioTrackDuplicationSerializationError> {
    let extensions = value
        .get("extensions")
        .and_then(Value::as_object)
        .ok_or(AudioTrackDuplicationSerializationError::InvalidDocumentShape)?;
    Ok(extensions.is_empty())
}

fn rewrite_string(
    value: &mut Value,
    key: &str,
    replacement: &str,
) -> Result<(), AudioTrackDuplicationSerializationError> {
    let object = value
        .as_object_mut()
        .ok_or(AudioTrackDuplicationSerializationError::InvalidDocumentShape)?;
    if !object.contains_key(key) {
        return Err(AudioTrackDuplicationSerializationError::InvalidDocumentShape);
    }
    object.insert(key.to_owned(), replacement.into());
    Ok(())
}

fn replace_u8(
    value: &mut Value,
    key: &str,
    replacement: u8,
) -> Result<(), AudioTrackDuplicationSerializationError> {
    let object = value
        .as_object_mut()
        .ok_or(AudioTrackDuplicationSerializationError::InvalidDocumentShape)?;
    if !object.contains_key(key) {
        return Err(AudioTrackDuplicationSerializationError::InvalidDocumentShape);
    }
    object.insert(key.to_owned(), u64::from(replacement).into());
    Ok(())
}

fn replace_string_array(
    value: &mut Value,
    key: &str,
    replacement: Vec<String>,
) -> Result<(), AudioTrackDuplicationSerializationError> {
    let object = value
        .as_object_mut()
        .ok_or(AudioTrackDuplicationSerializationError::InvalidDocumentShape)?;
    if !object.get(key).is_some_and(Value::is_array) {
        return Err(AudioTrackDuplicationSerializationError::InvalidDocumentShape);
    }
    object.insert(
        key.to_owned(),
        Value::Array(replacement.into_iter().map(Value::String).collect()),
    );
    Ok(())
}

#[allow(clippy::too_many_arguments, reason = "the helper carries one complete arrangement identity map")]
fn duplicate_arrangement(
    candidate: &mut LoadedPlaybackProject,
    project: &LoadedPlaybackProject,
    source: Option<&[u8]>,
    source_track_id: &str,
    duplicated_track_id: &str,
    cloned_clip_ids: &HashMap<&str, String>,
    known_identities: &mut BTreeSet<String>,
    next_identity: &mut impl FnMut() -> String,
    limits: LoaderLimits,
) -> Result<Option<AudioTrackDuplicationDocument>, AudioTrackDuplicationSerializationError> {
    let Some(source) = source else {
        return Ok(None);
    };
    let Some(original) = &project.arrangement else {
        return Err(AudioTrackDuplicationSerializationError::SourceArrangementMismatch);
    };
    let source_placements = original
        .placements
        .iter()
        .filter(|placement| placement.track_id == source_track_id)
        .collect::<Vec<_>>();
    if source_placements.is_empty() {
        return Ok(None);
    }
    if original
        .placements
        .len()
        .checked_add(source_placements.len())
        .is_none_or(|count| count > limits.max_placements)
    {
        return Err(AudioTrackDuplicationSerializationError::Source(
            super::domain_error_value(
                DocumentLocation {
                    kind: DocumentKind::Arrangement,
                    index: 0,
                },
                "track duplication exceeds arrangement placement capacity",
            ),
        ));
    }
    let (mut value, _) = canonical_value(source)?;
    require_empty_extensions(&value)?;
    let placements = value
        .get_mut("placements")
        .and_then(Value::as_array_mut)
        .ok_or(AudioTrackDuplicationSerializationError::InvalidDocumentShape)?;
    let mut additions = Vec::new();
    additions
        .try_reserve_exact(source_placements.len())
        .map_err(|_| AudioTrackDuplicationSerializationError::Source(capacity_error()))?;
    for placement in placements.iter().filter(|placement| {
        placement.get("track_id").and_then(Value::as_str) == Some(source_track_id)
    }) {
        let source_clip_id = placement
            .get("clip_id")
            .and_then(Value::as_str)
            .ok_or(AudioTrackDuplicationSerializationError::InvalidDocumentShape)?;
        let cloned_clip_id = cloned_clip_ids
            .get(source_clip_id)
            .ok_or(AudioTrackDuplicationSerializationError::SourceArrangementMismatch)?;
        let mut duplicate = placement.clone();
        rewrite_string(
            &mut duplicate,
            "id",
            &allocate_identity(known_identities, next_identity)?,
        )?;
        rewrite_string(&mut duplicate, "track_id", duplicated_track_id)?;
        rewrite_string(&mut duplicate, "clip_id", cloned_clip_id)?;
        additions.push(duplicate);
    }
    if additions.len() != source_placements.len() {
        return Err(AudioTrackDuplicationSerializationError::SourceArrangementMismatch);
    }
    placements.extend(additions);
    placements.sort_unstable_by(placement_order);
    let bytes = canonical_bytes(&value)?;
    let loaded = load_arrangement(
        &bytes,
        candidate.project_id(),
        limits.max_document_bytes,
        limits.max_placements,
        limits.max_scene_launches,
    )
    .map_err(AudioTrackDuplicationSerializationError::Source)?;
    candidate.arrangement = Some(loaded);
    Ok(Some(AudioTrackDuplicationDocument {
        id: original.id.clone(),
        bytes,
    }))
}

fn placement_order(left: &Value, right: &Value) -> std::cmp::Ordering {
    let left_start = left.get("start_tick").and_then(Value::as_u64);
    let right_start = right.get("start_tick").and_then(Value::as_u64);
    let left_track = left.get("track_id").and_then(Value::as_str);
    let right_track = right.get("track_id").and_then(Value::as_str);
    let left_id = left.get("id").and_then(Value::as_str);
    let right_id = right.get("id").and_then(Value::as_str);
    left_start
        .cmp(&right_start)
        .then_with(|| left_track.cmp(&right_track))
        .then_with(|| left_id.cmp(&right_id))
}

#[allow(clippy::too_many_arguments, reason = "the helper carries one complete scene-slot identity map")]
fn duplicate_scene_actions(
    candidate: &mut LoadedPlaybackProject,
    project: &LoadedPlaybackProject,
    source_scenes: &HashMap<&str, &[u8]>,
    source_track_id: &str,
    duplicated_track_id: &str,
    cloned_clip_ids: &HashMap<&str, String>,
    limits: LoaderLimits,
) -> Result<Vec<AudioTrackDuplicationDocument>, AudioTrackDuplicationSerializationError> {
    let mut output = Vec::new();
    for (scene_index, source_scene) in project.scenes.iter().enumerate() {
        let Some(source_slot) = source_scene
            .slots
            .iter()
            .find(|slot| slot.track_id == source_track_id)
        else {
            continue;
        };
        if source_scene.slots.len() >= limits.max_scene_slots {
            return Err(AudioTrackDuplicationSerializationError::SceneSlotCapacity);
        }
        let bytes = source_scenes
            .get(source_scene.id.as_str())
            .copied()
            .ok_or(AudioTrackDuplicationSerializationError::SourceSceneCoverage)?;
        let (mut value, _) = canonical_value(bytes)?;
        require_empty_extensions(&value)?;
        let slots = value
            .get_mut("slots")
            .and_then(Value::as_array_mut)
            .ok_or(AudioTrackDuplicationSerializationError::InvalidDocumentShape)?;
        let copied_action = match &source_slot.action {
            super::LoadedSceneSlotAction::Play { clip_id } => {
                let duplicated_clip_id = cloned_clip_ids
                    .get(clip_id.as_str())
                    .ok_or(AudioTrackDuplicationSerializationError::SourceSceneCoverage)?;
                serde_json::json!({
                    "track_id": duplicated_track_id,
                    "action": "play",
                    "clip_id": duplicated_clip_id,
                })
            }
            super::LoadedSceneSlotAction::Stop => serde_json::json!({
                "track_id": duplicated_track_id,
                "action": "stop",
            }),
        };
        slots.push(copied_action);
        let bytes = canonical_bytes(&value)?;
        let loaded = load_scene(
            &bytes,
            scene_index,
            candidate.project_id(),
            limits.max_document_bytes,
            limits.max_scene_slots,
        )
        .map_err(AudioTrackDuplicationSerializationError::Source)?;
        candidate.scenes[scene_index] = loaded;
        output.push(AudioTrackDuplicationDocument {
            id: source_scene.id.clone(),
            bytes,
        });
    }
    Ok(output)
}

#[allow(clippy::too_many_arguments, reason = "the helper validates one complete mixer replacement and its send identity map")]
fn duplicate_mixer(
    candidate: &LoadedPlaybackProject,
    source: &[u8],
    loaded_source: &super::LoadedMixer,
    source_track_id: &str,
    duplicated_track_id: &str,
    assets: &[MediaAssetFact<'_>],
    limits: LoaderLimits,
    known_identities: &mut BTreeSet<String>,
    next_identity: &mut impl FnMut() -> String,
) -> Result<
    (
        AudioTrackDuplicationDocument,
        HashMap<String, String>,
        super::LoadedMixer,
    ),
    AudioTrackDuplicationSerializationError,
> {
    let source_channel = loaded_source
        .channels
        .iter()
        .find(|channel| channel.track_id == source_track_id)
        .ok_or(AudioTrackDuplicationSerializationError::SourceMixerMismatch)?;
    let (mut value, _) = canonical_value(source)?;
    require_empty_extensions(&value)?;
    let channels = value
        .get_mut("channels")
        .and_then(Value::as_array_mut)
        .ok_or(AudioTrackDuplicationSerializationError::InvalidDocumentShape)?;
    let source_channel_index = channels
        .iter()
        .position(|channel| channel.get("track_id").and_then(Value::as_str) == Some(source_track_id))
        .ok_or(AudioTrackDuplicationSerializationError::SourceMixerMismatch)?;
    let mut duplicate = channels[source_channel_index].clone();
    rewrite_string(&mut duplicate, "track_id", duplicated_track_id)?;
    let sends = duplicate
        .get_mut("sends")
        .and_then(Value::as_array_mut)
        .ok_or(AudioTrackDuplicationSerializationError::InvalidDocumentShape)?;
    if sends.len() != source_channel.sends.len() {
        return Err(AudioTrackDuplicationSerializationError::SourceMixerMismatch);
    }
    let mut source_send_ids = HashMap::new();
    source_send_ids
        .try_reserve(sends.len())
        .map_err(|_| AudioTrackDuplicationSerializationError::Source(capacity_error()))?;
    for (index, send) in sends.iter_mut().enumerate() {
        let source_id = source_channel.sends[index].id.as_str();
        if send.get("id").and_then(Value::as_str) != Some(source_id) {
            return Err(AudioTrackDuplicationSerializationError::SourceMixerMismatch);
        }
        let duplicated_id = allocate_identity(known_identities, next_identity)?;
        rewrite_string(send, "id", &duplicated_id)?;
        source_send_ids.insert(source_id.to_owned(), duplicated_id);
    }
    let duplicated_channel_index = candidate
        .structural_track_index(duplicated_track_id)
        .ok_or(AudioTrackDuplicationSerializationError::SourceMixerMismatch)?;
    if duplicated_channel_index > channels.len() {
        return Err(AudioTrackDuplicationSerializationError::SourceMixerMismatch);
    }
    channels.insert(duplicated_channel_index, duplicate);
    let bytes = canonical_bytes(&value)?;
    let loaded = candidate
        .load_mixer(&bytes, assets, limits)
        .map_err(AudioTrackDuplicationSerializationError::Source)?;
    Ok((
        AudioTrackDuplicationDocument {
            id: loaded_source.id.clone(),
            bytes,
        },
        source_send_ids,
        loaded,
    ))
}

#[allow(clippy::too_many_arguments, reason = "the helper atomically rewrites every source-targeted automation lane")]
fn duplicate_automation_lanes(
    project: &LoadedPlaybackProject,
    assets: &[MediaAssetFact<'_>],
    source: &[(&str, &[u8])],
    source_track_id: &str,
    duplicated_track_id: &str,
    source_send_ids: &HashMap<String, String>,
    mixer: &super::LoadedMixer,
    limits: LoaderLimits,
    known_identities: &mut BTreeSet<String>,
    next_identity: &mut impl FnMut() -> String,
) -> Result<Vec<AudioTrackDuplicationDocument>, AudioTrackDuplicationSerializationError> {
    let mut rewritten = Vec::new();
    rewritten
        .try_reserve_exact(source.len())
        .map_err(|_| AudioTrackDuplicationSerializationError::Source(capacity_error()))?;
    let mut output = Vec::new();
    for (take_id, bytes) in source {
        let (mut value, canonical) = canonical_value(bytes)?;
        if canonical != *bytes {
            return Err(AudioTrackDuplicationSerializationError::SourceAutomationCoverage);
        }
        let extensions_empty = extensions_are_empty(&value)?;
        let lanes = value
            .get_mut("lanes")
            .and_then(Value::as_array_mut)
            .ok_or(AudioTrackDuplicationSerializationError::InvalidDocumentShape)?;
        let mut additions = Vec::new();
        for lane in lanes.iter() {
            let Some(target) = lane.get("target").and_then(Value::as_object) else {
                return Err(AudioTrackDuplicationSerializationError::InvalidDocumentShape);
            };
            let Some(kind) = target.get("kind").and_then(Value::as_str) else {
                return Err(AudioTrackDuplicationSerializationError::InvalidDocumentShape);
            };
            let rewrite = match kind {
                "mixer-channel"
                    if target.get("track_id").and_then(Value::as_str) == Some(source_track_id) =>
                {
                    Some(("track_id", duplicated_track_id.to_owned()))
                }
                "mixer-send" => target
                    .get("send_id")
                    .and_then(Value::as_str)
                    .and_then(|id| source_send_ids.get(id))
                    .map(|id| ("send_id", id.clone())),
                "mixer-channel" | "mixer-main" | "mixer-cue" => None,
                _ => return Err(AudioTrackDuplicationSerializationError::InvalidDocumentShape),
            };
            let Some((target_key, target_id)) = rewrite else {
                continue;
            };
            let mut duplicated_lane = lane.clone();
            rewrite_string(
                &mut duplicated_lane,
                "id",
                &allocate_identity(known_identities, next_identity)?,
            )?;
            let target = duplicated_lane
                .get_mut("target")
                .ok_or(AudioTrackDuplicationSerializationError::InvalidDocumentShape)?;
            rewrite_string(target, target_key, &target_id)?;
            additions.push(duplicated_lane);
        }
        if additions.is_empty() {
            rewritten.push(None);
        } else {
            if !extensions_empty {
                return Err(AudioTrackDuplicationSerializationError::UnknownExtensionSemantics);
            }
            lanes.extend(additions);
            let bytes = canonical_bytes(&value)?;
            output.push(AudioTrackDuplicationDocument {
                id: (*take_id).to_owned(),
                bytes: bytes.clone(),
            });
            rewritten.push(Some(bytes));
        }
    }
    let mut candidate_documents = Vec::new();
    candidate_documents
        .try_reserve_exact(source.len())
        .map_err(|_| AudioTrackDuplicationSerializationError::Source(capacity_error()))?;
    for ((_, source_bytes), replacement) in source.iter().zip(&rewritten) {
        candidate_documents.push(replacement.as_deref().unwrap_or(source_bytes));
    }
    let automation = project
        .load_automation_takes(mixer, assets, &candidate_documents, limits)
        .map_err(AudioTrackDuplicationSerializationError::Source)?;
    let graph = mixer
        .compile_main_graph()
        .map_err(AudioTrackDuplicationSerializationError::MixerCompilation)?;
    project
        .compile_main_automation(&graph, &automation)
        .map_err(AudioTrackDuplicationSerializationError::AutomationCompilation)?;
    Ok(output)
}

/// Complete verified source authority for routing-bus duplication.
///
/// Incoming routes and sends are deliberately not part of this source: those
/// edges belong to their source channels and a bus copy must not create new
/// signal paths without an explicit routing edit.
#[derive(Clone, Copy, Debug)]
pub struct RoutingBusDuplicationSource<'a> {
    pub track: &'a [u8],
    pub mixer: &'a [u8],
    pub automation_take_ids: &'a [&'a str],
    pub automation_takes: &'a [(&'a str, &'a [u8])],
    pub device_chain_id: Option<&'a str>,
    pub device_chain: Option<&'a [u8]>,
}

/// Rejection returned before an incomplete routing-bus copy can be published.
#[derive(Debug)]
pub enum RoutingBusDuplicationSerializationError {
    TrackMissing,
    TrackNotRoutingBus,
    ParentTrackUnsupported,
    TrackCapacity,
    ReturnBusUnavailable,
    InvalidTrackName,
    InvalidGeneratedIdentity,
    SourceTrackMismatch,
    SourceMixerMismatch,
    SourceAutomationCoverage,
    SourceDeviceChainMismatch,
    InvalidDocumentShape,
    UnknownExtensionSemantics,
    Source(LoadError),
    Device(DeviceChainError),
    Serialization(serde_json::Error),
    ProjectCompilation(AdapterCompileError),
    MixerCompilation(MixerCompileError),
    AutomationCompilation(AutomationCompileError),
}

impl fmt::Display for RoutingBusDuplicationSerializationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TrackMissing => formatter.write_str("the selected routing bus is not present"),
            Self::TrackNotRoutingBus => {
                formatter.write_str("routing-bus duplication supports group and return tracks only")
            }
            Self::ParentTrackUnsupported => {
                formatter.write_str("routing-bus duplication does not infer hierarchy copy semantics")
            }
            Self::TrackCapacity => formatter.write_str("the routing-bus capacity is exhausted"),
            Self::ReturnBusUnavailable => {
                formatter.write_str("no version-1 return-bus identity is available for the copy")
            }
            Self::InvalidTrackName => {
                formatter.write_str("the duplicated routing-bus name is outside the version-1 domain")
            }
            Self::InvalidGeneratedIdentity => {
                formatter.write_str("a generated duplication identity is invalid or collides")
            }
            Self::SourceTrackMismatch => {
                formatter.write_str("source routing-bus bytes do not match the loaded project")
            }
            Self::SourceMixerMismatch => {
                formatter.write_str("source mixer bytes do not match the loaded project")
            }
            Self::SourceAutomationCoverage => {
                formatter.write_str("source automation documents have duplicate or misaddressed identities")
            }
            Self::SourceDeviceChainMismatch => {
                formatter.write_str("source native device chain does not belong to the selected routing bus")
            }
            Self::InvalidDocumentShape => {
                formatter.write_str("a source document shape changed after typed validation")
            }
            Self::UnknownExtensionSemantics => formatter.write_str(
                "routing-bus duplication refuses a transformed document with unknown extension identity semantics",
            ),
            Self::Source(source) => source.fmt(formatter),
            Self::Device(source) => source.fmt(formatter),
            Self::Serialization(source) => source.fmt(formatter),
            Self::ProjectCompilation(source) => source.fmt(formatter),
            Self::MixerCompilation(source) => source.fmt(formatter),
            Self::AutomationCompilation(source) => source.fmt(formatter),
        }
    }
}

impl Error for RoutingBusDuplicationSerializationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Source(source) => Some(source),
            Self::Device(source) => Some(source),
            Self::Serialization(source) => Some(source),
            Self::ProjectCompilation(source) => Some(source),
            Self::MixerCompilation(source) => Some(source),
            Self::AutomationCompilation(source) => Some(source),
            Self::TrackMissing
            | Self::TrackNotRoutingBus
            | Self::ParentTrackUnsupported
            | Self::TrackCapacity
            | Self::ReturnBusUnavailable
            | Self::InvalidTrackName
            | Self::InvalidGeneratedIdentity
            | Self::SourceTrackMismatch
            | Self::SourceMixerMismatch
            | Self::SourceAutomationCoverage
            | Self::SourceDeviceChainMismatch
            | Self::InvalidDocumentShape
            | Self::UnknownExtensionSemantics => None,
        }
    }
}

/// Builds one independent Group or Return copy without copying the incoming
/// routing edges that belong to other channels.
///
/// # Errors
///
/// Returns a classified refusal for incomplete or stale authority, unsupported
/// topology, exhausted routing capacity, unknown extension semantics, unsafe
/// identities, or a candidate that fails playback or automation compilation.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "routing-bus duplication validates every owned mutable identity edge before returning one atomic document set"
)]
pub fn serialize_routing_bus_duplication(
    project: &LoadedPlaybackProject,
    assets: &[MediaAssetFact<'_>],
    source: RoutingBusDuplicationSource<'_>,
    source_track_id: &str,
    duplicated_track_name: &str,
    limits: LoaderLimits,
    mut next_identity: impl FnMut() -> String,
) -> Result<TrackDuplicationDocuments, RoutingBusDuplicationSerializationError> {
    if project.tracks.len() >= limits.max_track_documents {
        return Err(RoutingBusDuplicationSerializationError::TrackCapacity);
    }
    let name_length = duplicated_track_name.chars().count();
    if duplicated_track_name.trim() != duplicated_track_name
        || !(1..=super::MAX_VERSION_1_NAME_CHARACTERS).contains(&name_length)
    {
        return Err(RoutingBusDuplicationSerializationError::InvalidTrackName);
    }
    let source_track_index = project
        .track_index(source_track_id)
        .ok_or(RoutingBusDuplicationSerializationError::TrackMissing)?;
    let loaded_source_track = load_track(
        source.track,
        source_track_index,
        project.project_id(),
        limits.max_document_bytes,
        limits.max_track_clip_references,
    )
    .map_err(RoutingBusDuplicationSerializationError::Source)?;
    if !same_track(&loaded_source_track, &project.tracks[source_track_index]) {
        return Err(RoutingBusDuplicationSerializationError::SourceTrackMismatch);
    }
    let kind = match loaded_source_track.kind {
        super::TrackKind::Group => super::RoutingBusKind::Group,
        super::TrackKind::Return => super::RoutingBusKind::Return,
        _ => return Err(RoutingBusDuplicationSerializationError::TrackNotRoutingBus),
    };
    if loaded_source_track.parent_track_id.is_some() {
        return Err(RoutingBusDuplicationSerializationError::ParentTrackUnsupported);
    }
    let next_order = (0..=7)
        .find(|order| project.tracks.iter().all(|track| track.order != *order))
        .ok_or(RoutingBusDuplicationSerializationError::TrackCapacity)?;
    let (mut track_value, canonical_track) = routing_bus_canonical_value(source.track)?;
    if canonical_track != source.track {
        return Err(RoutingBusDuplicationSerializationError::SourceTrackMismatch);
    }
    routing_bus_require_empty_extensions(&track_value)?;

    let (mixer_value, canonical_mixer) = routing_bus_canonical_value(source.mixer)?;
    if canonical_mixer != source.mixer {
        return Err(RoutingBusDuplicationSerializationError::SourceMixerMismatch);
    }
    routing_bus_require_empty_extensions(&mixer_value)?;
    let loaded_mixer = project
        .load_mixer(source.mixer, assets, limits)
        .map_err(RoutingBusDuplicationSerializationError::Source)?;
    let source_automation = routing_bus_source_automation(
        project,
        &loaded_mixer,
        assets,
        source.automation_take_ids,
        source.automation_takes,
        limits,
    )?;
    let mut known_identities = known_identities(project, assets, &loaded_mixer, &source_automation);
    let parsed_chain = routing_bus_source_device_chain(
        project,
        source,
        source_track_id,
        &mut known_identities,
    )?;
    let duplicated_track_id = routing_bus_allocate_identity(&mut known_identities, &mut next_identity)?;
    routing_bus_rewrite_string(&mut track_value, "id", &duplicated_track_id)?;
    routing_bus_rewrite_string(&mut track_value, "name", duplicated_track_name)?;
    routing_bus_replace_u8(&mut track_value, "order", next_order)?;
    let track_bytes = routing_bus_canonical_bytes(&track_value)?;
    let duplicated_track = load_track(
        &track_bytes,
        project.tracks.len(),
        project.project_id(),
        limits.max_document_bytes,
        limits.max_track_clip_references,
    )
    .map_err(RoutingBusDuplicationSerializationError::Source)?;
    let mut candidate = project.clone();
    candidate
        .tracks
        .try_reserve(1)
        .map_err(|_| RoutingBusDuplicationSerializationError::Source(capacity_error()))?;
    candidate.tracks.push(duplicated_track);
    validate_track_hierarchy(&candidate.tracks).map_err(RoutingBusDuplicationSerializationError::Source)?;
    validate_loaded_identities(&candidate).map_err(RoutingBusDuplicationSerializationError::Source)?;

    let (mixer, source_send_ids, candidate_mixer) = duplicate_routing_bus_mixer(
        &candidate,
        source.mixer,
        &loaded_mixer,
        source_track_id,
        &duplicated_track_id,
        kind,
        assets,
        limits,
        &mut known_identities,
        &mut next_identity,
    )?;
    let automation_takes = duplicate_routing_bus_automation_lanes(
        &candidate,
        assets,
        source.automation_takes,
        source_track_id,
        &duplicated_track_id,
        &source_send_ids,
        &candidate_mixer,
        limits,
        &mut known_identities,
        &mut next_identity,
    )?;
    candidate
        .compile_playback(assets, GraphLimits::VERSION_1, SceneLimits::VERSION_1)
        .map_err(RoutingBusDuplicationSerializationError::ProjectCompilation)?;
    let device_chain = parsed_chain
        .map(|chain| {
            let chain_id = routing_bus_allocate_identity(&mut known_identities, &mut next_identity)?;
            let mut device_ids = Vec::new();
            device_ids
                .try_reserve_exact(chain.devices().len())
                .map_err(|_| RoutingBusDuplicationSerializationError::Source(capacity_error()))?;
            for _ in chain.devices() {
                device_ids.push(routing_bus_allocate_identity(&mut known_identities, &mut next_identity)?);
            }
            let cloned = chain
                .try_clone_for_track(chain_id.clone(), duplicated_track_id.clone(), &device_ids)
                .map_err(RoutingBusDuplicationSerializationError::Device)?;
            let bytes = cloned
                .to_canonical_vec()
                .map_err(RoutingBusDuplicationSerializationError::Device)?;
            Ok(TrackDuplicationDocument { id: chain_id, bytes })
        })
        .transpose()?;
    Ok(TrackDuplicationDocuments {
        track: TrackDuplicationDocument {
            id: duplicated_track_id,
            bytes: track_bytes,
        },
        clips: Vec::new(),
        mixer,
        arrangement: None,
        scenes: Vec::new(),
        automation_takes,
        device_chain,
    })
}

fn routing_bus_source_automation(
    project: &LoadedPlaybackProject,
    mixer: &super::LoadedMixer,
    assets: &[MediaAssetFact<'_>],
    expected_ids: &[&str],
    source: &[(&str, &[u8])],
    limits: LoaderLimits,
) -> Result<super::LoadedAutomationSet, RoutingBusDuplicationSerializationError> {
    if expected_ids.len() != source.len() {
        return Err(RoutingBusDuplicationSerializationError::SourceAutomationCoverage);
    }
    let mut expected = BTreeSet::new();
    for id in expected_ids {
        if !is_valid_entity_id(id) || !expected.insert(*id) {
            return Err(RoutingBusDuplicationSerializationError::SourceAutomationCoverage);
        }
    }
    let mut documents = Vec::new();
    documents
        .try_reserve_exact(source.len())
        .map_err(|_| RoutingBusDuplicationSerializationError::Source(capacity_error()))?;
    let mut actual = BTreeSet::new();
    for (id, bytes) in source {
        if !actual.insert(*id) || !expected.contains(*id) {
            return Err(RoutingBusDuplicationSerializationError::SourceAutomationCoverage);
        }
        let (_, canonical) = routing_bus_canonical_value(bytes)?;
        if canonical != *bytes {
            return Err(RoutingBusDuplicationSerializationError::SourceAutomationCoverage);
        }
        documents.push(*bytes);
    }
    let loaded = project
        .load_automation_takes(mixer, assets, &documents, limits)
        .map_err(RoutingBusDuplicationSerializationError::Source)?;
    if loaded.takes().len() != source.len()
        || loaded
            .takes()
            .iter()
            .zip(source)
            .any(|(take, (id, _))| take.id() != *id)
    {
        return Err(RoutingBusDuplicationSerializationError::SourceAutomationCoverage);
    }
    Ok(loaded)
}

fn routing_bus_source_device_chain(
    project: &LoadedPlaybackProject,
    source: RoutingBusDuplicationSource<'_>,
    source_track_id: &str,
    known_identities: &mut BTreeSet<String>,
) -> Result<Option<DeviceChain>, RoutingBusDuplicationSerializationError> {
    match (source.device_chain_id, source.device_chain) {
        (None, None) => Ok(None),
        (Some(_), None) | (None, Some(_)) => {
            Err(RoutingBusDuplicationSerializationError::SourceDeviceChainMismatch)
        }
        (Some(expected_id), Some(bytes)) => {
            let (_, canonical) = routing_bus_canonical_value(bytes)?;
            if canonical != bytes {
                return Err(RoutingBusDuplicationSerializationError::SourceDeviceChainMismatch);
            }
            let chain = DeviceChain::parse(bytes).map_err(RoutingBusDuplicationSerializationError::Device)?;
            if chain.id() != expected_id
                || chain.project_id() != project.project_id()
                || chain.track_id() != source_track_id
                || !known_identities.insert(chain.id().to_owned())
                || chain
                    .devices()
                    .iter()
                    .any(|device| !known_identities.insert(device.id().to_owned()))
            {
                return Err(RoutingBusDuplicationSerializationError::SourceDeviceChainMismatch);
            }
            Ok(Some(chain))
        }
    }
}

#[allow(clippy::too_many_arguments, reason = "the helper carries the owned channel, send, and return-bus identity transformations together")]
fn duplicate_routing_bus_mixer(
    candidate: &LoadedPlaybackProject,
    source: &[u8],
    loaded_source: &super::LoadedMixer,
    source_track_id: &str,
    duplicated_track_id: &str,
    kind: super::RoutingBusKind,
    assets: &[MediaAssetFact<'_>],
    limits: LoaderLimits,
    known_identities: &mut BTreeSet<String>,
    next_identity: &mut impl FnMut() -> String,
) -> Result<
    (
        TrackDuplicationDocument,
        HashMap<String, String>,
        super::LoadedMixer,
    ),
    RoutingBusDuplicationSerializationError,
> {
    let source_channel = loaded_source
        .channels
        .iter()
        .find(|channel| channel.track_id == source_track_id)
        .ok_or(RoutingBusDuplicationSerializationError::SourceMixerMismatch)?;
    let (mut value, _) = routing_bus_canonical_value(source)?;
    routing_bus_require_empty_extensions(&value)?;
    let (duplicate, source_send_ids) = {
        let channels = value
            .get_mut("channels")
            .and_then(Value::as_array_mut)
            .ok_or(RoutingBusDuplicationSerializationError::InvalidDocumentShape)?;
        let source_index = channels
            .iter()
            .position(|channel| channel.get("track_id").and_then(Value::as_str) == Some(source_track_id))
            .ok_or(RoutingBusDuplicationSerializationError::SourceMixerMismatch)?;
        let mut duplicate = channels[source_index].clone();
        routing_bus_rewrite_string(&mut duplicate, "track_id", duplicated_track_id)?;
        let sends = duplicate
            .get_mut("sends")
            .and_then(Value::as_array_mut)
            .ok_or(RoutingBusDuplicationSerializationError::InvalidDocumentShape)?;
        if sends.len() != source_channel.sends.len() {
            return Err(RoutingBusDuplicationSerializationError::SourceMixerMismatch);
        }
        let mut source_send_ids = HashMap::new();
        source_send_ids
            .try_reserve(sends.len())
            .map_err(|_| RoutingBusDuplicationSerializationError::Source(capacity_error()))?;
        for (index, send) in sends.iter_mut().enumerate() {
            let source_id = source_channel.sends[index].id.as_str();
            if send.get("id").and_then(Value::as_str) != Some(source_id) {
                return Err(RoutingBusDuplicationSerializationError::SourceMixerMismatch);
            }
            let duplicated_id = routing_bus_allocate_identity(known_identities, next_identity)?;
            routing_bus_rewrite_string(send, "id", &duplicated_id)?;
            source_send_ids.insert(source_id.to_owned(), duplicated_id);
        }
        (duplicate, source_send_ids)
    };
    if kind == super::RoutingBusKind::Return {
        let return_buses = value
            .get_mut("return_buses")
            .and_then(Value::as_array_mut)
            .ok_or(RoutingBusDuplicationSerializationError::InvalidDocumentShape)?;
        let used = return_buses
            .iter()
            .map(|entry| entry.get("bus").and_then(Value::as_str))
            .collect::<Option<BTreeSet<_>>>()
            .ok_or(RoutingBusDuplicationSerializationError::InvalidDocumentShape)?;
        let bus = ["a", "b"]
            .into_iter()
            .find(|bus| !used.contains(bus))
            .ok_or(RoutingBusDuplicationSerializationError::ReturnBusUnavailable)?;
        return_buses.push(serde_json::json!({"bus": bus, "track_id": duplicated_track_id}));
        return_buses.sort_by_key(|entry| entry["bus"].as_str().unwrap_or_default().to_owned());
    }
    let channel_index = candidate
        .structural_track_index(duplicated_track_id)
        .ok_or(RoutingBusDuplicationSerializationError::SourceMixerMismatch)?;
    let channels = value
        .get_mut("channels")
        .and_then(Value::as_array_mut)
        .ok_or(RoutingBusDuplicationSerializationError::InvalidDocumentShape)?;
    if channel_index > channels.len() {
        return Err(RoutingBusDuplicationSerializationError::SourceMixerMismatch);
    }
    channels.insert(channel_index, duplicate);
    let bytes = routing_bus_canonical_bytes(&value)?;
    let loaded = candidate
        .load_mixer(&bytes, assets, limits)
        .map_err(RoutingBusDuplicationSerializationError::Source)?;
    Ok((
        TrackDuplicationDocument {
            id: loaded_source.id.clone(),
            bytes,
        },
        source_send_ids,
        loaded,
    ))
}

#[allow(clippy::too_many_arguments, reason = "the helper rewrites every owned routing-bus automation target in one candidate mixer domain")]
fn duplicate_routing_bus_automation_lanes(
    project: &LoadedPlaybackProject,
    assets: &[MediaAssetFact<'_>],
    source: &[(&str, &[u8])],
    source_track_id: &str,
    duplicated_track_id: &str,
    source_send_ids: &HashMap<String, String>,
    mixer: &super::LoadedMixer,
    limits: LoaderLimits,
    known_identities: &mut BTreeSet<String>,
    next_identity: &mut impl FnMut() -> String,
) -> Result<Vec<TrackDuplicationDocument>, RoutingBusDuplicationSerializationError> {
    let mut rewritten = Vec::new();
    rewritten
        .try_reserve_exact(source.len())
        .map_err(|_| RoutingBusDuplicationSerializationError::Source(capacity_error()))?;
    let mut output = Vec::new();
    for (take_id, bytes) in source {
        let (mut value, canonical) = routing_bus_canonical_value(bytes)?;
        if canonical != *bytes {
            return Err(RoutingBusDuplicationSerializationError::SourceAutomationCoverage);
        }
        let extensions_empty = routing_bus_extensions_empty(&value)?;
        let lanes = value
            .get_mut("lanes")
            .and_then(Value::as_array_mut)
            .ok_or(RoutingBusDuplicationSerializationError::InvalidDocumentShape)?;
        let mut additions = Vec::new();
        for lane in lanes.iter() {
            let target = lane
                .get("target")
                .and_then(Value::as_object)
                .ok_or(RoutingBusDuplicationSerializationError::InvalidDocumentShape)?;
            let kind = target
                .get("kind")
                .and_then(Value::as_str)
                .ok_or(RoutingBusDuplicationSerializationError::InvalidDocumentShape)?;
            let rewrite = match kind {
                "mixer-channel"
                    if target.get("track_id").and_then(Value::as_str) == Some(source_track_id) =>
                {
                    Some(("track_id", duplicated_track_id.to_owned()))
                }
                "mixer-send" => target
                    .get("send_id")
                    .and_then(Value::as_str)
                    .and_then(|id| source_send_ids.get(id))
                    .map(|id| ("send_id", id.clone())),
                "mixer-channel" | "mixer-main" | "mixer-cue" => None,
                _ => return Err(RoutingBusDuplicationSerializationError::InvalidDocumentShape),
            };
            let Some((target_key, target_id)) = rewrite else {
                continue;
            };
            let mut duplicate = lane.clone();
            routing_bus_rewrite_string(
                &mut duplicate,
                "id",
                &routing_bus_allocate_identity(known_identities, next_identity)?,
            )?;
            routing_bus_rewrite_string(
                duplicate
                    .get_mut("target")
                    .ok_or(RoutingBusDuplicationSerializationError::InvalidDocumentShape)?,
                target_key,
                &target_id,
            )?;
            additions.push(duplicate);
        }
        if additions.is_empty() {
            rewritten.push(None);
        } else {
            if !extensions_empty {
                return Err(RoutingBusDuplicationSerializationError::UnknownExtensionSemantics);
            }
            lanes.extend(additions);
            let bytes = routing_bus_canonical_bytes(&value)?;
            output.push(TrackDuplicationDocument {
                id: (*take_id).to_owned(),
                bytes: bytes.clone(),
            });
            rewritten.push(Some(bytes));
        }
    }
    let candidate_documents = rewritten
        .iter()
        .zip(source)
        .map(|(replacement, (_, source_bytes))| replacement.as_deref().unwrap_or(source_bytes))
        .collect::<Vec<_>>();
    let automation = project
        .load_automation_takes(mixer, assets, &candidate_documents, limits)
        .map_err(RoutingBusDuplicationSerializationError::Source)?;
    let graph = mixer
        .compile_main_graph()
        .map_err(RoutingBusDuplicationSerializationError::MixerCompilation)?;
    project
        .compile_main_automation(&graph, &automation)
        .map_err(RoutingBusDuplicationSerializationError::AutomationCompilation)?;
    Ok(output)
}

fn routing_bus_allocate_identity(
    known: &mut BTreeSet<String>,
    next_identity: &mut impl FnMut() -> String,
) -> Result<String, RoutingBusDuplicationSerializationError> {
    let identity = next_identity();
    if !is_valid_entity_id(&identity) || !known.insert(identity.clone()) {
        return Err(RoutingBusDuplicationSerializationError::InvalidGeneratedIdentity);
    }
    Ok(identity)
}

fn routing_bus_canonical_value(
    bytes: &[u8],
) -> Result<(Value, Vec<u8>), RoutingBusDuplicationSerializationError> {
    let value = serde_json::from_slice(bytes).map_err(RoutingBusDuplicationSerializationError::Serialization)?;
    let canonical = routing_bus_canonical_bytes(&value)?;
    Ok((value, canonical))
}

fn routing_bus_canonical_bytes(
    value: &Value,
) -> Result<Vec<u8>, RoutingBusDuplicationSerializationError> {
    serde_json_canonicalizer::to_vec(value).map_err(RoutingBusDuplicationSerializationError::Serialization)
}

fn routing_bus_extensions_empty(
    value: &Value,
) -> Result<bool, RoutingBusDuplicationSerializationError> {
    value
        .get("extensions")
        .and_then(Value::as_object)
        .map(serde_json::Map::is_empty)
        .ok_or(RoutingBusDuplicationSerializationError::InvalidDocumentShape)
}

fn routing_bus_require_empty_extensions(
    value: &Value,
) -> Result<(), RoutingBusDuplicationSerializationError> {
    if routing_bus_extensions_empty(value)? {
        Ok(())
    } else {
        Err(RoutingBusDuplicationSerializationError::UnknownExtensionSemantics)
    }
}

fn routing_bus_rewrite_string(
    value: &mut Value,
    key: &str,
    replacement: &str,
) -> Result<(), RoutingBusDuplicationSerializationError> {
    let object = value
        .as_object_mut()
        .ok_or(RoutingBusDuplicationSerializationError::InvalidDocumentShape)?;
    if !object.contains_key(key) {
        return Err(RoutingBusDuplicationSerializationError::InvalidDocumentShape);
    }
    object.insert(key.to_owned(), replacement.into());
    Ok(())
}

fn routing_bus_replace_u8(
    value: &mut Value,
    key: &str,
    replacement: u8,
) -> Result<(), RoutingBusDuplicationSerializationError> {
    let object = value
        .as_object_mut()
        .ok_or(RoutingBusDuplicationSerializationError::InvalidDocumentShape)?;
    if !object.contains_key(key) {
        return Err(RoutingBusDuplicationSerializationError::InvalidDocumentShape);
    }
    object.insert(key.to_owned(), u64::from(replacement).into());
    Ok(())
}

/// One canonical replacement or removed resource in a complete track-content
/// deletion transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioTrackContentRemovalDocument {
    id: String,
    bytes: Vec<u8>,
}

impl AudioTrackContentRemovalDocument {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Complete verified source authority for a destructive audio-track removal.
///
/// Automation resources are supplied explicitly because they are stored
/// outside the playback-document graph. The source therefore cannot silently
/// leave a lane addressing a removed channel or send behind.
#[derive(Clone, Copy, Debug)]
pub struct AudioTrackContentRemovalSource<'a> {
    pub track: &'a [u8],
    pub clips: &'a [(&'a str, &'a [u8])],
    pub scenes: &'a [(&'a str, &'a [u8])],
    pub arrangement: Option<&'a [u8]>,
    pub mixer: &'a [u8],
    pub automation_take_ids: &'a [&'a str],
    pub automation_takes: &'a [(&'a str, &'a [u8])],
    pub device_chain_id: Option<&'a str>,
    pub device_chain: Option<&'a [u8]>,
}

/// Complete resource delta for removing one flat audio track and all of its
/// mutable musical state. Immutable project media is intentionally absent: it
/// remains available to the separate unused-media cleanup authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioTrackContentRemovalDocuments {
    track: AudioTrackContentRemovalDocument,
    clips: Vec<AudioTrackContentRemovalDocument>,
    mixer: AudioTrackContentRemovalDocument,
    arrangement: Option<AudioTrackContentRemovalDocument>,
    scenes: Vec<AudioTrackContentRemovalDocument>,
    automation_takes: Vec<AudioTrackContentRemovalDocument>,
    device_chain: Option<AudioTrackContentRemovalDocument>,
}

impl AudioTrackContentRemovalDocuments {
    #[must_use]
    pub const fn track(&self) -> &AudioTrackContentRemovalDocument {
        &self.track
    }

    #[must_use]
    pub fn clips(&self) -> &[AudioTrackContentRemovalDocument] {
        &self.clips
    }

    #[must_use]
    pub const fn mixer(&self) -> &AudioTrackContentRemovalDocument {
        &self.mixer
    }

    #[must_use]
    pub const fn arrangement(&self) -> Option<&AudioTrackContentRemovalDocument> {
        self.arrangement.as_ref()
    }

    #[must_use]
    pub fn scenes(&self) -> &[AudioTrackContentRemovalDocument] {
        &self.scenes
    }

    #[must_use]
    pub fn automation_takes(&self) -> &[AudioTrackContentRemovalDocument] {
        &self.automation_takes
    }

    #[must_use]
    pub const fn device_chain(&self) -> Option<&AudioTrackContentRemovalDocument> {
        self.device_chain.as_ref()
    }
}

/// Rejection returned before a partially reduced track graph can be published.
#[derive(Debug)]
pub enum AudioTrackContentRemovalSerializationError {
    TrackMissing,
    TrackNotAudio,
    ParentTrackUnsupported,
    SourceTrackMismatch,
    SourceClipCoverage,
    SourceSceneCoverage,
    SourceArrangementMismatch,
    SourceMixerMismatch,
    SourceAutomationCoverage,
    SourceDeviceChainMismatch,
    InvalidDocumentShape,
    UnknownExtensionSemantics,
    Source(LoadError),
    Device(DeviceChainError),
    Serialization(serde_json::Error),
    ProjectCompilation(AdapterCompileError),
    MixerCompilation(MixerCompileError),
    AutomationCompilation(AutomationCompileError),
}

impl fmt::Display for AudioTrackContentRemovalSerializationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TrackMissing => formatter.write_str("the selected track is not present"),
            Self::TrackNotAudio => formatter.write_str("track removal supports audio tracks only"),
            Self::ParentTrackUnsupported => {
                formatter.write_str("track removal does not infer hierarchy deletion semantics")
            }
            Self::SourceTrackMismatch => {
                formatter.write_str("source track bytes do not match the loaded project")
            }
            Self::SourceClipCoverage => {
                formatter.write_str("source clip documents do not exactly cover the selected track")
            }
            Self::SourceSceneCoverage => {
                formatter.write_str("source scene documents do not exactly cover the project")
            }
            Self::SourceArrangementMismatch => {
                formatter.write_str("source arrangement bytes do not match the loaded project")
            }
            Self::SourceMixerMismatch => {
                formatter.write_str("source mixer bytes do not match the loaded project")
            }
            Self::SourceAutomationCoverage => {
                formatter.write_str("source automation documents have duplicate or misaddressed identities")
            }
            Self::SourceDeviceChainMismatch => {
                formatter.write_str("source native device chain does not belong to the selected track")
            }
            Self::InvalidDocumentShape => {
                formatter.write_str("a source document shape changed after typed validation")
            }
            Self::UnknownExtensionSemantics => formatter.write_str(
                "track removal refuses a transformed document with unknown extension semantics",
            ),
            Self::Source(source) => source.fmt(formatter),
            Self::Device(source) => source.fmt(formatter),
            Self::Serialization(source) => source.fmt(formatter),
            Self::ProjectCompilation(source) => source.fmt(formatter),
            Self::MixerCompilation(source) => source.fmt(formatter),
            Self::AutomationCompilation(source) => source.fmt(formatter),
        }
    }
}

impl Error for AudioTrackContentRemovalSerializationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Source(source) => Some(source),
            Self::Device(source) => Some(source),
            Self::Serialization(source) => Some(source),
            Self::ProjectCompilation(source) => Some(source),
            Self::MixerCompilation(source) => Some(source),
            Self::AutomationCompilation(source) => Some(source),
            Self::TrackMissing
            | Self::TrackNotAudio
            | Self::ParentTrackUnsupported
            | Self::SourceTrackMismatch
            | Self::SourceClipCoverage
            | Self::SourceSceneCoverage
            | Self::SourceArrangementMismatch
            | Self::SourceMixerMismatch
            | Self::SourceAutomationCoverage
            | Self::SourceDeviceChainMismatch
            | Self::InvalidDocumentShape
            | Self::UnknownExtensionSemantics => None,
        }
    }
}

/// Builds the complete mutable document delta for one confirmed audio-track
/// deletion. The result deliberately carries both prior bytes for every
/// removed resource and canonical replacements for every dependent resource;
/// storage must publish it as one manifest-last transaction.
///
/// # Errors
///
/// Returns a classified refusal when the supplied authority is incomplete,
/// stale, noncanonical, unsupported, or cannot compile after removal.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the deletion boundary validates every mutable edge before returning one all-or-nothing resource delta"
)]
pub fn serialize_audio_track_content_removal(
    project: &LoadedPlaybackProject,
    assets: &[MediaAssetFact<'_>],
    source: AudioTrackContentRemovalSource<'_>,
    source_track_id: &str,
    limits: LoaderLimits,
) -> Result<AudioTrackContentRemovalDocuments, AudioTrackContentRemovalSerializationError> {
    let source_track_index = project
        .track_index(source_track_id)
        .ok_or(AudioTrackContentRemovalSerializationError::TrackMissing)?;
    let loaded_source_track = load_track(
        source.track,
        source_track_index,
        project.project_id(),
        limits.max_document_bytes,
        limits.max_track_clip_references,
    )
    .map_err(AudioTrackContentRemovalSerializationError::Source)?;
    let expected_source_track = &project.tracks[source_track_index];
    if !same_track(&loaded_source_track, expected_source_track) {
        return Err(AudioTrackContentRemovalSerializationError::SourceTrackMismatch);
    }
    if loaded_source_track.kind != super::TrackKind::Audio {
        return Err(AudioTrackContentRemovalSerializationError::TrackNotAudio);
    }
    if loaded_source_track.parent_track_id.is_some()
        || project
            .tracks
            .iter()
            .any(|track| track.parent_track_id.as_deref() == Some(source_track_id))
    {
        return Err(AudioTrackContentRemovalSerializationError::ParentTrackUnsupported);
    }
    let (track_value, canonical_track) = removal_canonical_value(source.track)?;
    if canonical_track != source.track || !removal_extensions_empty(&track_value)? {
        return Err(AudioTrackContentRemovalSerializationError::UnknownExtensionSemantics);
    }

    let source_clips = removal_source_clip_map(project, &loaded_source_track, source.clips, limits)?;
    let source_scenes = removal_source_scene_map(project, source.scenes, limits)?;
    let source_arrangement = removal_source_arrangement(project, source.arrangement, limits)?;

    let (mixer_value, canonical_mixer) = removal_canonical_value(source.mixer)?;
    if canonical_mixer != source.mixer {
        return Err(AudioTrackContentRemovalSerializationError::SourceMixerMismatch);
    }
    if !removal_extensions_empty(&mixer_value)? {
        return Err(AudioTrackContentRemovalSerializationError::UnknownExtensionSemantics);
    }
    let loaded_mixer = project
        .load_mixer(source.mixer, assets, limits)
        .map_err(AudioTrackContentRemovalSerializationError::Source)?;
    let source_channel = loaded_mixer
        .channels
        .iter()
        .find(|channel| channel.track_id == source_track_id)
        .ok_or(AudioTrackContentRemovalSerializationError::SourceMixerMismatch)?;
    let source_send_ids: BTreeSet<&str> = source_channel
        .sends
        .iter()
        .map(|send| send.id.as_str())
        .collect();

    let source_automation = removal_source_automation(
        project,
        &loaded_mixer,
        assets,
        source.automation_take_ids,
        source.automation_takes,
        limits,
    )?;
    let device_chain = removal_source_device_chain(project, source, source_track_id)?;

    let mut candidate = project.clone();
    candidate.tracks.remove(source_track_index);
    candidate
        .clips
        .retain(|clip| clip.track_id != source_track_id);
    validate_track_hierarchy(&candidate.tracks)
        .map_err(AudioTrackContentRemovalSerializationError::Source)?;

    let arrangement = removal_arrangement(
        &mut candidate,
        project,
        source_arrangement,
        source_track_id,
        limits,
    )?;
    let scenes = removal_scene_actions(
        &mut candidate,
        project,
        &source_scenes,
        source_track_id,
        limits,
    )?;
    validate_loaded_identities(&candidate).map_err(AudioTrackContentRemovalSerializationError::Source)?;

    let mixer = removal_mixer(
        &candidate,
        source.mixer,
        &loaded_mixer,
        source_track_id,
        assets,
        limits,
    )?;
    let candidate_mixer = candidate
        .load_mixer(mixer.bytes(), assets, limits)
        .map_err(AudioTrackContentRemovalSerializationError::Source)?;
    let automation_takes = removal_automation_lanes(
        project,
        assets,
        source.automation_takes,
        &source_automation,
        source_track_id,
        &source_send_ids,
        &candidate_mixer,
        limits,
    )?;
    candidate
        .compile_playback(assets, GraphLimits::VERSION_1, SceneLimits::VERSION_1)
        .map_err(AudioTrackContentRemovalSerializationError::ProjectCompilation)?;

    let clips = loaded_source_track
        .clip_ids
        .iter()
        .map(|id| {
            source_clips
                .get(id.as_str())
                .copied()
                .map(|bytes| AudioTrackContentRemovalDocument {
                    id: id.clone(),
                    bytes: bytes.to_vec(),
                })
                .ok_or(AudioTrackContentRemovalSerializationError::SourceClipCoverage)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(AudioTrackContentRemovalDocuments {
        track: AudioTrackContentRemovalDocument {
            id: loaded_source_track.id,
            bytes: source.track.to_vec(),
        },
        clips,
        mixer,
        arrangement,
        scenes,
        automation_takes,
        device_chain,
    })
}

fn removal_canonical_value(
    bytes: &[u8],
) -> Result<(Value, Vec<u8>), AudioTrackContentRemovalSerializationError> {
    let value = serde_json::from_slice(bytes)
        .map_err(AudioTrackContentRemovalSerializationError::Serialization)?;
    let canonical = serde_json_canonicalizer::to_vec(&value)
        .map_err(AudioTrackContentRemovalSerializationError::Serialization)?;
    Ok((value, canonical))
}

fn removal_extensions_empty(
    value: &Value,
) -> Result<bool, AudioTrackContentRemovalSerializationError> {
    value
        .get("extensions")
        .and_then(Value::as_object)
        .map(serde_json::Map::is_empty)
        .ok_or(AudioTrackContentRemovalSerializationError::InvalidDocumentShape)
}

fn removal_source_clip_map<'a>(
    project: &LoadedPlaybackProject,
    source_track: &super::LoadedTrack,
    source: &'a [(&'a str, &'a [u8])],
    limits: LoaderLimits,
) -> Result<HashMap<&'a str, &'a [u8]>, AudioTrackContentRemovalSerializationError> {
    if source.len() != source_track.clip_ids.len() {
        return Err(AudioTrackContentRemovalSerializationError::SourceClipCoverage);
    }
    let mut output = HashMap::new();
    output
        .try_reserve(source.len())
        .map_err(|_| AudioTrackContentRemovalSerializationError::Source(capacity_error()))?;
    for (clip_id, bytes) in source {
        let clip_index = project
            .clips
            .iter()
            .position(|clip| clip.id == *clip_id)
            .ok_or(AudioTrackContentRemovalSerializationError::SourceClipCoverage)?;
        if output.insert(*clip_id, *bytes).is_some()
            || !source_track.clip_ids.iter().any(|id| id == clip_id)
        {
            return Err(AudioTrackContentRemovalSerializationError::SourceClipCoverage);
        }
        let (value, canonical) = removal_canonical_value(bytes)?;
        if canonical != *bytes || !removal_extensions_empty(&value)? {
            return Err(AudioTrackContentRemovalSerializationError::UnknownExtensionSemantics);
        }
        let loaded = load_clip(
            bytes,
            clip_index,
            project.project_id(),
            limits.max_document_bytes,
            limits.max_warp_markers,
        )
        .map_err(AudioTrackContentRemovalSerializationError::Source)?;
        if loaded.id != *clip_id
            || loaded.track_id != source_track.id
            || loaded != project.clips[clip_index]
        {
            return Err(AudioTrackContentRemovalSerializationError::SourceClipCoverage);
        }
    }
    if output.len() != source_track.clip_ids.len() {
        return Err(AudioTrackContentRemovalSerializationError::SourceClipCoverage);
    }
    Ok(output)
}

fn removal_source_scene_map<'a>(
    project: &LoadedPlaybackProject,
    source: &'a [(&'a str, &'a [u8])],
    limits: LoaderLimits,
) -> Result<HashMap<&'a str, &'a [u8]>, AudioTrackContentRemovalSerializationError> {
    if source.len() != project.scenes.len() {
        return Err(AudioTrackContentRemovalSerializationError::SourceSceneCoverage);
    }
    let mut output = HashMap::new();
    output
        .try_reserve(source.len())
        .map_err(|_| AudioTrackContentRemovalSerializationError::Source(capacity_error()))?;
    for (id, bytes) in source {
        let scene_index = project
            .scenes
            .iter()
            .position(|scene| scene.id == *id)
            .ok_or(AudioTrackContentRemovalSerializationError::SourceSceneCoverage)?;
        if output.insert(*id, *bytes).is_some() {
            return Err(AudioTrackContentRemovalSerializationError::SourceSceneCoverage);
        }
        let (_, canonical) = removal_canonical_value(bytes)?;
        if canonical != *bytes {
            return Err(AudioTrackContentRemovalSerializationError::SourceSceneCoverage);
        }
        let loaded = load_scene(
            bytes,
            scene_index,
            project.project_id(),
            limits.max_document_bytes,
            limits.max_scene_slots,
        )
        .map_err(AudioTrackContentRemovalSerializationError::Source)?;
        if loaded.id != *id || loaded != project.scenes[scene_index] {
            return Err(AudioTrackContentRemovalSerializationError::SourceSceneCoverage);
        }
    }
    Ok(output)
}

fn removal_source_arrangement<'a>(
    project: &LoadedPlaybackProject,
    source: Option<&'a [u8]>,
    limits: LoaderLimits,
) -> Result<Option<&'a [u8]>, AudioTrackContentRemovalSerializationError> {
    match (project.arrangement.as_ref(), source) {
        (None, None) => Ok(None),
        (Some(_), None) | (None, Some(_)) => {
            Err(AudioTrackContentRemovalSerializationError::SourceArrangementMismatch)
        }
        (Some(expected), Some(bytes)) => {
            let (_, canonical) = removal_canonical_value(bytes)?;
            if canonical != bytes {
                return Err(AudioTrackContentRemovalSerializationError::SourceArrangementMismatch);
            }
            let loaded = load_arrangement(
                bytes,
                project.project_id(),
                limits.max_document_bytes,
                limits.max_placements,
                limits.max_scene_launches,
            )
            .map_err(AudioTrackContentRemovalSerializationError::Source)?;
            if loaded != *expected {
                return Err(AudioTrackContentRemovalSerializationError::SourceArrangementMismatch);
            }
            Ok(Some(bytes))
        }
    }
}

fn removal_source_automation(
    project: &LoadedPlaybackProject,
    mixer: &super::LoadedMixer,
    assets: &[MediaAssetFact<'_>],
    expected_ids: &[&str],
    source: &[(&str, &[u8])],
    limits: LoaderLimits,
) -> Result<super::LoadedAutomationSet, AudioTrackContentRemovalSerializationError> {
    if expected_ids.len() != source.len() {
        return Err(AudioTrackContentRemovalSerializationError::SourceAutomationCoverage);
    }
    let mut expected = BTreeSet::new();
    for id in expected_ids {
        if !is_valid_entity_id(id) || !expected.insert(*id) {
            return Err(AudioTrackContentRemovalSerializationError::SourceAutomationCoverage);
        }
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(source.len())
        .map_err(|_| AudioTrackContentRemovalSerializationError::Source(capacity_error()))?;
    let mut actual = BTreeSet::new();
    for (id, document) in source {
        if !actual.insert(*id) || !expected.contains(*id) {
            return Err(AudioTrackContentRemovalSerializationError::SourceAutomationCoverage);
        }
        let (_, canonical) = removal_canonical_value(document)?;
        if canonical != *document {
            return Err(AudioTrackContentRemovalSerializationError::SourceAutomationCoverage);
        }
        bytes.push(*document);
    }
    let automation = project
        .load_automation_takes(mixer, assets, &bytes, limits)
        .map_err(AudioTrackContentRemovalSerializationError::Source)?;
    if automation.takes().len() != source.len()
        || automation
            .takes()
            .iter()
            .zip(source)
            .any(|(take, (id, _))| take.id() != *id)
    {
        return Err(AudioTrackContentRemovalSerializationError::SourceAutomationCoverage);
    }
    Ok(automation)
}

fn removal_source_device_chain(
    project: &LoadedPlaybackProject,
    source: AudioTrackContentRemovalSource<'_>,
    source_track_id: &str,
) -> Result<Option<AudioTrackContentRemovalDocument>, AudioTrackContentRemovalSerializationError> {
    match (source.device_chain_id, source.device_chain) {
        (None, None) => Ok(None),
        (Some(_), None) | (None, Some(_)) => {
            Err(AudioTrackContentRemovalSerializationError::SourceDeviceChainMismatch)
        }
        (Some(id), Some(bytes)) => {
            let chain = DeviceChain::parse(bytes)
                .map_err(AudioTrackContentRemovalSerializationError::Device)?;
            if chain.id() != id
                || chain.project_id() != project.project_id()
                || chain.track_id() != source_track_id
            {
                return Err(AudioTrackContentRemovalSerializationError::SourceDeviceChainMismatch);
            }
            Ok(Some(AudioTrackContentRemovalDocument {
                id: id.to_owned(),
                bytes: bytes.to_vec(),
            }))
        }
    }
}

fn removal_arrangement(
    candidate: &mut LoadedPlaybackProject,
    project: &LoadedPlaybackProject,
    source: Option<&[u8]>,
    source_track_id: &str,
    limits: LoaderLimits,
) -> Result<Option<AudioTrackContentRemovalDocument>, AudioTrackContentRemovalSerializationError> {
    let Some(source) = source else {
        return Ok(None);
    };
    let Some(original) = &project.arrangement else {
        return Err(AudioTrackContentRemovalSerializationError::SourceArrangementMismatch);
    };
    if !original
        .placements
        .iter()
        .any(|placement| placement.track_id == source_track_id)
    {
        return Ok(None);
    }
    let (mut value, _) = removal_canonical_value(source)?;
    if !removal_extensions_empty(&value)? {
        return Err(AudioTrackContentRemovalSerializationError::UnknownExtensionSemantics);
    }
    let placements = value
        .get_mut("placements")
        .and_then(Value::as_array_mut)
        .ok_or(AudioTrackContentRemovalSerializationError::InvalidDocumentShape)?;
    let previous = placements.len();
    placements.retain(|placement| {
        placement.get("track_id").and_then(Value::as_str) != Some(source_track_id)
    });
    if placements.len() >= previous {
        return Err(AudioTrackContentRemovalSerializationError::SourceArrangementMismatch);
    }
    let bytes = serde_json_canonicalizer::to_vec(&value)
        .map_err(AudioTrackContentRemovalSerializationError::Serialization)?;
    let loaded = load_arrangement(
        &bytes,
        candidate.project_id(),
        limits.max_document_bytes,
        limits.max_placements,
        limits.max_scene_launches,
    )
    .map_err(AudioTrackContentRemovalSerializationError::Source)?;
    candidate.arrangement = Some(loaded);
    Ok(Some(AudioTrackContentRemovalDocument {
        id: original.id.clone(),
        bytes,
    }))
}

fn removal_scene_actions(
    candidate: &mut LoadedPlaybackProject,
    project: &LoadedPlaybackProject,
    source_scenes: &HashMap<&str, &[u8]>,
    source_track_id: &str,
    limits: LoaderLimits,
) -> Result<Vec<AudioTrackContentRemovalDocument>, AudioTrackContentRemovalSerializationError> {
    let mut output = Vec::new();
    for (index, scene) in project.scenes.iter().enumerate() {
        if !scene.slots.iter().any(|slot| slot.track_id == source_track_id) {
            continue;
        }
        let source = source_scenes
            .get(scene.id.as_str())
            .copied()
            .ok_or(AudioTrackContentRemovalSerializationError::SourceSceneCoverage)?;
        let (mut value, _) = removal_canonical_value(source)?;
        if !removal_extensions_empty(&value)? {
            return Err(AudioTrackContentRemovalSerializationError::UnknownExtensionSemantics);
        }
        let slots = value
            .get_mut("slots")
            .and_then(Value::as_array_mut)
            .ok_or(AudioTrackContentRemovalSerializationError::InvalidDocumentShape)?;
        let previous = slots.len();
        slots.retain(|slot| slot.get("track_id").and_then(Value::as_str) != Some(source_track_id));
        if slots.len() >= previous {
            return Err(AudioTrackContentRemovalSerializationError::SourceSceneCoverage);
        }
        let bytes = serde_json_canonicalizer::to_vec(&value)
            .map_err(AudioTrackContentRemovalSerializationError::Serialization)?;
        candidate.scenes[index] = load_scene(
            &bytes,
            index,
            candidate.project_id(),
            limits.max_document_bytes,
            limits.max_scene_slots,
        )
        .map_err(AudioTrackContentRemovalSerializationError::Source)?;
        output.push(AudioTrackContentRemovalDocument {
            id: scene.id.clone(),
            bytes,
        });
    }
    Ok(output)
}

fn removal_mixer(
    candidate: &LoadedPlaybackProject,
    source: &[u8],
    loaded_source: &super::LoadedMixer,
    source_track_id: &str,
    assets: &[MediaAssetFact<'_>],
    limits: LoaderLimits,
) -> Result<AudioTrackContentRemovalDocument, AudioTrackContentRemovalSerializationError> {
    let (mut value, _) = removal_canonical_value(source)?;
    let channels = value
        .get_mut("channels")
        .and_then(Value::as_array_mut)
        .ok_or(AudioTrackContentRemovalSerializationError::InvalidDocumentShape)?;
    let source_index = channels
        .iter()
        .position(|channel| channel.get("track_id").and_then(Value::as_str) == Some(source_track_id))
        .ok_or(AudioTrackContentRemovalSerializationError::SourceMixerMismatch)?;
    if source_index >= loaded_source.channels.len()
        || loaded_source.channels[source_index].track_id != source_track_id
    {
        return Err(AudioTrackContentRemovalSerializationError::SourceMixerMismatch);
    }
    channels.remove(source_index);
    let bytes = serde_json_canonicalizer::to_vec(&value)
        .map_err(AudioTrackContentRemovalSerializationError::Serialization)?;
    candidate
        .load_mixer(&bytes, assets, limits)
        .map_err(AudioTrackContentRemovalSerializationError::Source)?;
    Ok(AudioTrackContentRemovalDocument {
        id: loaded_source.id.clone(),
        bytes,
    })
}

#[allow(clippy::too_many_arguments, reason = "the helper keeps complete automation coverage and the removed target domain together")]
fn removal_automation_lanes(
    project: &LoadedPlaybackProject,
    assets: &[MediaAssetFact<'_>],
    source: &[(&str, &[u8])],
    loaded_source: &super::LoadedAutomationSet,
    source_track_id: &str,
    source_send_ids: &BTreeSet<&str>,
    mixer: &super::LoadedMixer,
    limits: LoaderLimits,
) -> Result<Vec<AudioTrackContentRemovalDocument>, AudioTrackContentRemovalSerializationError> {
    let mut output = Vec::new();
    let mut candidate_documents: Vec<Vec<u8>> = Vec::new();
    candidate_documents
        .try_reserve_exact(source.len())
        .map_err(|_| AudioTrackContentRemovalSerializationError::Source(capacity_error()))?;
    for ((take_id, source_bytes), loaded_take) in source.iter().zip(loaded_source.takes()) {
        if loaded_take.id() != *take_id {
            return Err(AudioTrackContentRemovalSerializationError::SourceAutomationCoverage);
        }
        let (mut value, _) = removal_canonical_value(source_bytes)?;
        let extensions_empty = removal_extensions_empty(&value)?;
        let lanes = value
            .get_mut("lanes")
            .and_then(Value::as_array_mut)
            .ok_or(AudioTrackContentRemovalSerializationError::InvalidDocumentShape)?;
        let mut removed = false;
        let mut retained = Vec::new();
        retained
            .try_reserve_exact(lanes.len())
            .map_err(|_| AudioTrackContentRemovalSerializationError::Source(capacity_error()))?;
        for lane in lanes.iter() {
            let target = lane
                .get("target")
                .and_then(Value::as_object)
                .ok_or(AudioTrackContentRemovalSerializationError::InvalidDocumentShape)?;
            let remove = match target.get("kind").and_then(Value::as_str) {
                Some("mixer-channel") => {
                    target.get("track_id").and_then(Value::as_str) == Some(source_track_id)
                }
                Some("mixer-send") => target
                    .get("send_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| source_send_ids.contains(id)),
                Some("mixer-main" | "mixer-cue") => false,
                Some(_) | None => {
                    return Err(AudioTrackContentRemovalSerializationError::InvalidDocumentShape);
                }
            };
            removed |= remove;
            if !remove {
                retained.push(lane.clone());
            }
        }
        if !removed {
            candidate_documents.push((*source_bytes).to_vec());
            continue;
        }
        if !extensions_empty {
            return Err(AudioTrackContentRemovalSerializationError::UnknownExtensionSemantics);
        }
        *lanes = retained;
        let bytes = serde_json_canonicalizer::to_vec(&value)
            .map_err(AudioTrackContentRemovalSerializationError::Serialization)?;
        candidate_documents.push(bytes.clone());
        output.push(AudioTrackContentRemovalDocument {
            id: (*take_id).to_owned(),
            bytes,
        });
    }
    let candidate_refs: Vec<&[u8]> = candidate_documents.iter().map(Vec::as_slice).collect();
    let automation = project
        .load_automation_takes(mixer, assets, &candidate_refs, limits)
        .map_err(AudioTrackContentRemovalSerializationError::Source)?;
    let graph = mixer
        .compile_main_graph()
        .map_err(AudioTrackContentRemovalSerializationError::MixerCompilation)?;
    project
        .compile_main_automation(&graph, &automation)
        .map_err(AudioTrackContentRemovalSerializationError::AutomationCompilation)?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper_native_device::{
        NativeDeviceInstance, NativeFilterMode, NativeFilterState,
    };

    const PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122335001";
    const TRACK_ID: &str = "018f47a2-6b2d-7c11-8a44-001122335002";
    const RETURN_ID: &str = "018f47a2-6b2d-7c11-8a44-001122335003";
    const CLIP_ID: &str = "018f47a2-6b2d-7c11-8a44-001122335004";
    const ASSET_ID: &str = "018f47a2-6b2d-7c11-8a44-001122335005";
    const SCENE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122335006";
    const ARRANGEMENT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122335007";
    const PLACEMENT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122335008";
    const MIXER_ID: &str = "018f47a2-6b2d-7c11-8a44-001122335009";
    const SEND_ID: &str = "018f47a2-6b2d-7c11-8a44-001122335010";
    const TAKE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122335011";
    const CHANNEL_LANE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122335012";
    const SEND_LANE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122335013";
    const MAIN_LANE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122335014";

    #[allow(
        clippy::needless_pass_by_value,
        reason = "test fixtures deliberately consume short JSON builders before canonicalization"
    )]
    fn canonical(value: serde_json::Value) -> Vec<u8> {
        serde_json_canonicalizer::to_vec(&value).unwrap()
    }

    #[allow(
        clippy::type_complexity,
        reason = "the compact fixture returns the complete independently-owned resource inventory"
    )]
    fn fixture() -> (
        Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>,
    ) {
        let timeline = canonical(serde_json::json!({
            "format":"hyper-timeline", "format_version":1,
            "id":"018f47a2-6b2d-7c11-8a44-001122335015", "project_id":PROJECT_ID,
            "ticks_per_quarter":1920,
            "tempo_events":[{"tick":0,"microseconds_per_quarter":1_000_000}],
            "meter_events":[{"tick":0,"numerator":4,"denominator":4}], "extensions":{}
        }));
        let track = canonical(serde_json::json!({
            "format":"hyper-track", "format_version":1, "id":TRACK_ID,
            "project_id":PROJECT_ID, "name":"Lead", "kind":"audio", "order":0,
            "active":true, "clip_ids":[CLIP_ID], "extensions":{}
        }));
        let return_track = canonical(serde_json::json!({
            "format":"hyper-track", "format_version":1, "id":RETURN_ID,
            "project_id":PROJECT_ID, "name":"Verb", "kind":"return", "order":1,
            "active":true, "clip_ids":[], "extensions":{}
        }));
        let clip = canonical(serde_json::json!({
            "format":"hyper-audio-clip", "format_version":1, "id":CLIP_ID,
            "project_id":PROJECT_ID, "track_id":TRACK_ID, "name":"Lead clip", "enabled":true,
            "duration_ticks":960,
            "source":{"asset_id":ASSET_ID,"start_frame":0,"frame_count":24000},
            "loop":{"enabled":false,"start_tick":0,"end_tick":960},
            "playback":{"gain_decibels":0,"reverse":false,"transpose_cents":0},
            "fades":{"curve":"linear-amplitude","in_ticks":0,"out_ticks":0},
            "warp_markers":[{"tick":0,"source_frame_offset":0},{"tick":960,"source_frame_offset":24000}],
            "extensions":{}
        }));
        let scene = canonical(serde_json::json!({
            "format":"hyper-scene", "format_version":1, "id":SCENE_ID,
            "project_id":PROJECT_ID, "name":"Verse", "order":0, "enabled":true,
            "launch_quantization":{"mode":"immediate"},
            "slots":[{"track_id":TRACK_ID,"action":"play","clip_id":CLIP_ID}], "extensions":{}
        }));
        let arrangement = canonical(serde_json::json!({
            "format":"hyper-arrangement", "format_version":1, "id":ARRANGEMENT_ID,
            "project_id":PROJECT_ID, "name":"Main", "enabled":true,
            "placements":[{"id":PLACEMENT_ID,"track_id":TRACK_ID,"clip_id":CLIP_ID,
                "start_tick":0,"clip_start_tick":0,"duration_ticks":960,"enabled":true}],
            "scene_launches":[], "extensions":{}
        }));
        let mixer = canonical(serde_json::json!({
            "format":"hyper-mixer", "format_version":1, "id":MIXER_ID, "project_id":PROJECT_ID,
            "channels":[
                {"track_id":TRACK_ID,"gain_millionths":800_000,"pan_millionths":-250_000,"mute":false,"solo":true,
                    "destination":{"kind":"main"},"sends":[{"id":SEND_ID,"bus":"a","tap":"post-fader","gain_millionths":500_000,"pan_millionths":100_000,"enabled":true}]},
                {"track_id":RETURN_ID,"gain_millionths":1_000_000,"pan_millionths":0,"mute":false,"solo":false,"destination":{"kind":"main"},"sends":[]}
            ],
            "return_buses":[{"bus":"a","track_id":RETURN_ID}],
            "main":{"gain_millionths":1_000_000,"mute":false},
            "cue":{"source":"main","gain_millionths":1_000_000,"mute":false}, "extensions":{}
        }));
        let take = canonical(serde_json::json!({
            "format":"hyper-automation-take", "format_version":1, "id":TAKE_ID,
            "project_id":PROJECT_ID, "name":"Lead automation", "enabled":true,
            "captured_at_utc":"2026-08-23T12:00:00.000Z", "start_tick":0, "end_tick":960,
            "lanes":[
                {"id":CHANNEL_LANE_ID,"target":{"kind":"mixer-channel","track_id":TRACK_ID,"parameter":"gain"},"interpolation":"linear","points":[{"tick":0,"value":800_000},{"tick":959,"value":900_000}]},
                {"id":SEND_LANE_ID,"target":{"kind":"mixer-send","send_id":SEND_ID,"parameter":"gain"},"interpolation":"hold","points":[{"tick":0,"value":500_000}]},
                {"id":MAIN_LANE_ID,"target":{"kind":"mixer-main","parameter":"gain"},"interpolation":"hold","points":[{"tick":0,"value":1_000_000}]}
            ], "extensions":{}
        }));
        let device = NativeDeviceInstance::try_new_filter(
            "018f47a2-6b2d-7c11-8a44-001122335016".to_owned(),
            true,
            NativeFilterState::try_new(NativeFilterMode::LowPass, 12_000_000, 1_000_000).unwrap(),
        )
        .unwrap();
        let device_chain = DeviceChain::try_new(
            "018f47a2-6b2d-7c11-8a44-001122335017".to_owned(),
            PROJECT_ID.to_owned(),
            TRACK_ID.to_owned(),
            vec![device],
        )
        .unwrap()
        .to_canonical_vec()
        .unwrap();
        (timeline, track, return_track, clip, scene, arrangement, mixer, take, device_chain)
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the one complete graph fixture asserts every independent mutable identity edge"
    )]
    fn duplicates_the_complete_audio_track_identity_graph() {
        let (timeline, track, return_track, clip, scene, arrangement, mixer, take, device_chain) = fixture();
        let source_track = track.clone();
        let source_clip = clip.clone();
        let source_scene = scene.clone();
        let source_arrangement = arrangement.clone();
        let source_mixer = mixer.clone();
        let source_take = take.clone();
        let tracks = [track.as_slice(), return_track.as_slice()];
        let clips = [clip.as_slice()];
        let scenes = [scene.as_slice()];
        let project = crate::load(
            crate::PlaybackJsonInput {
                project_id: PROJECT_ID,
                sample_rate_hz: 48_000,
                timeline: &timeline,
                tracks: &tracks,
                clips: &clips,
                scenes: &scenes,
                arrangement: Some(&arrangement),
            },
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let assets = [MediaAssetFact {
            id: ASSET_ID,
            sample_rate_hz: 48_000,
            frame_count: 24_000,
        }];
        let identities = [
            "018f47a2-6b2d-7c11-8a44-001122335101",
            "018f47a2-6b2d-7c11-8a44-001122335102",
            "018f47a2-6b2d-7c11-8a44-001122335103",
            "018f47a2-6b2d-7c11-8a44-001122335104",
            "018f47a2-6b2d-7c11-8a44-001122335105",
            "018f47a2-6b2d-7c11-8a44-001122335106",
            "018f47a2-6b2d-7c11-8a44-001122335107",
            "018f47a2-6b2d-7c11-8a44-001122335108",
        ];
        let mut identities = identities.into_iter();
        let documents = serialize_audio_track_duplication(
            &project,
            &assets,
            AudioTrackDuplicationSource {
                track: &track,
                clips: &[(CLIP_ID, clip.as_slice())],
                scenes: &[(SCENE_ID, scene.as_slice())],
                arrangement: Some(&arrangement),
                mixer: &mixer,
                automation_take_ids: &[TAKE_ID],
                automation_takes: &[(TAKE_ID, take.as_slice())],
                device_chain_id: Some("018f47a2-6b2d-7c11-8a44-001122335017"),
                device_chain: Some(&device_chain),
            },
            TRACK_ID,
            "Lead copy",
            LoaderLimits::VERSION_1,
            || identities.next().unwrap().to_owned(),
        )
        .unwrap();

        assert_eq!(documents.track().id(), "018f47a2-6b2d-7c11-8a44-001122335101");
        assert_eq!(documents.clips().len(), 1);
        assert_eq!(documents.scenes().len(), 1);
        assert_eq!(documents.automation_takes().len(), 1);
        assert_eq!(documents.device_chain().unwrap().id(), "018f47a2-6b2d-7c11-8a44-001122335107");
        assert_eq!(track, source_track);
        assert_eq!(clip, source_clip);
        assert_eq!(scene, source_scene);
        assert_eq!(arrangement, source_arrangement);
        assert_eq!(mixer, source_mixer);
        assert_eq!(take, source_take);

        let cloned_track = documents.track().bytes();
        let cloned_clip = documents.clips()[0].bytes();
        let cloned_scene = documents.scenes()[0].bytes();
        let cloned_arrangement = documents.arrangement().unwrap().bytes();
        let cloned_tracks = [track.as_slice(), return_track.as_slice(), cloned_track];
        let cloned_clips = [clip.as_slice(), cloned_clip];
        let cloned_scenes = [cloned_scene];
        let candidate = crate::load(
            crate::PlaybackJsonInput {
                project_id: PROJECT_ID,
                sample_rate_hz: 48_000,
                timeline: &timeline,
                tracks: &cloned_tracks,
                clips: &cloned_clips,
                scenes: &cloned_scenes,
                arrangement: Some(cloned_arrangement),
            },
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let candidate_mixer = candidate
            .load_mixer(documents.mixer().bytes(), &assets, LoaderLimits::VERSION_1)
            .unwrap();
        let automation = candidate
            .load_automation_takes(
                &candidate_mixer,
                &assets,
                &[documents.automation_takes()[0].bytes()],
                LoaderLimits::VERSION_1,
            )
            .unwrap();
        candidate
            .compile_main_automation(&candidate_mixer.compile_main_graph().unwrap(), &automation)
            .unwrap();
        assert_eq!(candidate.track_name(documents.track().id()), Some("Lead copy"));
        assert_eq!(candidate.clip_track_id_at(1), Some(documents.track().id()));
        assert_eq!(candidate.clip_asset_id_at(1), Some(ASSET_ID));
        let track_value: Value = serde_json::from_slice(cloned_track).unwrap();
        assert_eq!(track_value["clip_ids"], serde_json::json!(["018f47a2-6b2d-7c11-8a44-001122335102"]));
        let arrangement_value: Value = serde_json::from_slice(cloned_arrangement).unwrap();
        assert_eq!(arrangement_value["placements"].as_array().unwrap().len(), 2);
        assert_eq!(
            arrangement_value["placements"][1],
            serde_json::json!({
                "id":"018f47a2-6b2d-7c11-8a44-001122335103",
                "track_id":"018f47a2-6b2d-7c11-8a44-001122335101",
                "clip_id":"018f47a2-6b2d-7c11-8a44-001122335102",
                "start_tick":0,"clip_start_tick":0,"duration_ticks":960,"enabled":true
            })
        );
        let scene_value: Value = serde_json::from_slice(cloned_scene).unwrap();
        assert_eq!(scene_value["slots"].as_array().unwrap().len(), 2);
        assert_eq!(
            scene_value["slots"][1],
            serde_json::json!({
                "track_id":"018f47a2-6b2d-7c11-8a44-001122335101",
                "action":"play", "clip_id":"018f47a2-6b2d-7c11-8a44-001122335102"
            })
        );
        let mixer_value: Value = serde_json::from_slice(documents.mixer().bytes()).unwrap();
        assert_eq!(mixer_value["channels"][2]["track_id"], "018f47a2-6b2d-7c11-8a44-001122335101");
        assert_eq!(mixer_value["channels"][2]["sends"][0]["id"], "018f47a2-6b2d-7c11-8a44-001122335104");
        let automation_value: Value = serde_json::from_slice(documents.automation_takes()[0].bytes()).unwrap();
        assert_eq!(automation_value["lanes"].as_array().unwrap().len(), 5);
        assert_eq!(automation_value["lanes"][3]["id"], "018f47a2-6b2d-7c11-8a44-001122335105");
        assert_eq!(automation_value["lanes"][3]["target"]["track_id"], "018f47a2-6b2d-7c11-8a44-001122335101");
        assert_eq!(automation_value["lanes"][4]["id"], "018f47a2-6b2d-7c11-8a44-001122335106");
        assert_eq!(automation_value["lanes"][4]["target"]["send_id"], "018f47a2-6b2d-7c11-8a44-001122335104");
        let cloned_chain = DeviceChain::parse(documents.device_chain().unwrap().bytes()).unwrap();
        assert_eq!(cloned_chain.track_id(), documents.track().id());
        assert_eq!(cloned_chain.devices()[0].id(), "018f47a2-6b2d-7c11-8a44-001122335108");
    }

    #[test]
    fn duplication_uses_a_free_sparse_track_order_slot() {
        let (timeline, track, return_track, clip, scene, arrangement, mixer, take, _) = fixture();
        let mut sparse_return: Value = serde_json::from_slice(&return_track).unwrap();
        sparse_return["order"] = 7.into();
        let sparse_return = canonical(sparse_return);
        let tracks = [track.as_slice(), sparse_return.as_slice()];
        let clips = [clip.as_slice()];
        let scenes = [scene.as_slice()];
        let project = crate::load(
            crate::PlaybackJsonInput {
                project_id: PROJECT_ID,
                sample_rate_hz: 48_000,
                timeline: &timeline,
                tracks: &tracks,
                clips: &clips,
                scenes: &scenes,
                arrangement: Some(&arrangement),
            },
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let assets = [MediaAssetFact {
            id: ASSET_ID,
            sample_rate_hz: 48_000,
            frame_count: 24_000,
        }];
        let identities = [
            "018f47a2-6b2d-7c11-8a44-001122335201",
            "018f47a2-6b2d-7c11-8a44-001122335202",
            "018f47a2-6b2d-7c11-8a44-001122335203",
            "018f47a2-6b2d-7c11-8a44-001122335204",
            "018f47a2-6b2d-7c11-8a44-001122335205",
            "018f47a2-6b2d-7c11-8a44-001122335206",
        ];
        let mut identities = identities.into_iter();
        let documents = serialize_audio_track_duplication(
            &project,
            &assets,
            AudioTrackDuplicationSource {
                track: &track,
                clips: &[(CLIP_ID, clip.as_slice())],
                scenes: &[(SCENE_ID, scene.as_slice())],
                arrangement: Some(&arrangement),
                mixer: &mixer,
                automation_take_ids: &[TAKE_ID],
                automation_takes: &[(TAKE_ID, take.as_slice())],
                device_chain_id: None,
                device_chain: None,
            },
            TRACK_ID,
            "Lead copy",
            LoaderLimits::VERSION_1,
            || identities.next().unwrap().to_owned(),
        )
        .unwrap();
        let copied: Value = serde_json::from_slice(documents.track().bytes()).unwrap();
        assert_eq!(copied["order"], 1);
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the fixture proves a copied group retains only its owned identity graph and never creates a new inbound route"
    )]
    fn duplicates_group_owned_state_without_copying_incoming_routes() {
        let (timeline, track, return_track, clip, scene, arrangement, mixer, take, device_chain) = fixture();
        let mut group_track: Value = serde_json::from_slice(&return_track).unwrap();
        group_track["kind"] = "group".into();
        group_track["name"] = "Drums".into();
        let group_track = canonical(group_track);
        let return_two_id = "018f47a2-6b2d-7c11-8a44-001122335018";
        let mut second_return: Value = serde_json::from_slice(&return_track).unwrap();
        second_return["id"] = return_two_id.into();
        second_return["name"] = "Verb".into();
        second_return["order"] = 2.into();
        let second_return = canonical(second_return);
        let mut group_mixer: Value = serde_json::from_slice(&mixer).unwrap();
        group_mixer["channels"][0]["destination"] = serde_json::json!({"kind":"track", "track_id": RETURN_ID});
        group_mixer["channels"][0]["sends"] = serde_json::json!([]);
        group_mixer["channels"][1]["track_id"] = RETURN_ID.into();
        group_mixer["channels"][1]["sends"] = serde_json::json!([
            {"id": SEND_ID, "bus":"a", "tap":"post-fader", "gain_millionths":500_000, "pan_millionths":100_000, "enabled":true}
        ]);
        group_mixer["channels"].as_array_mut().unwrap().push(serde_json::json!({
            "track_id": return_two_id, "gain_millionths":1_000_000, "pan_millionths":0,
            "mute":false, "solo":false, "destination":{"kind":"main"}, "sends":[]
        }));
        group_mixer["return_buses"] = serde_json::json!([{"bus":"a", "track_id":return_two_id}]);
        let group_mixer = canonical(group_mixer);
        let mut group_take: Value = serde_json::from_slice(&take).unwrap();
        group_take["lanes"][0]["target"]["track_id"] = RETURN_ID.into();
        group_take["lanes"][1]["target"]["send_id"] = SEND_ID.into();
        let group_take = canonical(group_take);
        let mut group_chain: Value = serde_json::from_slice(&device_chain).unwrap();
        group_chain["track_id"] = RETURN_ID.into();
        let group_chain = canonical(group_chain);
        let tracks = [track.as_slice(), group_track.as_slice(), second_return.as_slice()];
        let clips = [clip.as_slice()];
        let scenes = [scene.as_slice()];
        let project = crate::load(
            crate::PlaybackJsonInput {
                project_id: PROJECT_ID,
                sample_rate_hz: 48_000,
                timeline: &timeline,
                tracks: &tracks,
                clips: &clips,
                scenes: &scenes,
                arrangement: Some(&arrangement),
            },
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let assets = [MediaAssetFact {
            id: ASSET_ID,
            sample_rate_hz: 48_000,
            frame_count: 24_000,
        }];
        let identities = [
            "018f47a2-6b2d-7c11-8a44-001122335101",
            "018f47a2-6b2d-7c11-8a44-001122335102",
            "018f47a2-6b2d-7c11-8a44-001122335103",
            "018f47a2-6b2d-7c11-8a44-001122335104",
            "018f47a2-6b2d-7c11-8a44-001122335105",
            "018f47a2-6b2d-7c11-8a44-001122335106",
        ];
        let mut identities = identities.into_iter();
        let documents = serialize_routing_bus_duplication(
            &project,
            &assets,
            RoutingBusDuplicationSource {
                track: &group_track,
                mixer: &group_mixer,
                automation_take_ids: &[TAKE_ID],
                automation_takes: &[(TAKE_ID, group_take.as_slice())],
                device_chain_id: Some("018f47a2-6b2d-7c11-8a44-001122335017"),
                device_chain: Some(&group_chain),
            },
            RETURN_ID,
            "Drums copy",
            LoaderLimits::VERSION_1,
            || identities.next().unwrap().to_owned(),
        )
        .unwrap();

        let group_copy_id = documents.track().id();
        let mixer_value: Value = serde_json::from_slice(documents.mixer().bytes()).unwrap();
        assert_eq!(mixer_value["channels"][0]["destination"]["track_id"], RETURN_ID);
        let group_copy = mixer_value["channels"]
            .as_array()
            .unwrap()
            .iter()
            .find(|channel| channel["track_id"] == group_copy_id)
            .unwrap();
        assert_eq!(group_copy["sends"][0]["id"], "018f47a2-6b2d-7c11-8a44-001122335102");
        assert_eq!(group_copy["sends"][0]["bus"], "a");
        let automation_value: Value = serde_json::from_slice(documents.automation_takes()[0].bytes()).unwrap();
        assert_eq!(automation_value["lanes"].as_array().unwrap().len(), 5);
        assert_eq!(automation_value["lanes"][3]["target"]["track_id"], group_copy_id);
        assert_eq!(automation_value["lanes"][4]["target"]["send_id"], "018f47a2-6b2d-7c11-8a44-001122335102");
        let cloned_chain = DeviceChain::parse(documents.device_chain().unwrap().bytes()).unwrap();
        assert_eq!(cloned_chain.track_id(), group_copy_id);
        assert_eq!(cloned_chain.devices()[0].id(), "018f47a2-6b2d-7c11-8a44-001122335106");
    }

    #[test]
    fn duplicates_return_with_a_fresh_bus_without_copying_its_incoming_sends() {
        let (timeline, track, return_track, clip, scene, arrangement, mixer, take, device_chain) = fixture();
        let mut return_chain: Value = serde_json::from_slice(&device_chain).unwrap();
        return_chain["track_id"] = RETURN_ID.into();
        let return_chain = canonical(return_chain);
        let tracks = [track.as_slice(), return_track.as_slice()];
        let clips = [clip.as_slice()];
        let scenes = [scene.as_slice()];
        let project = crate::load(
            crate::PlaybackJsonInput {
                project_id: PROJECT_ID,
                sample_rate_hz: 48_000,
                timeline: &timeline,
                tracks: &tracks,
                clips: &clips,
                scenes: &scenes,
                arrangement: Some(&arrangement),
            },
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let assets = [MediaAssetFact {
            id: ASSET_ID,
            sample_rate_hz: 48_000,
            frame_count: 24_000,
        }];
        let identities = [
            "018f47a2-6b2d-7c11-8a44-001122335201",
            "018f47a2-6b2d-7c11-8a44-001122335202",
            "018f47a2-6b2d-7c11-8a44-001122335203",
        ];
        let mut identities = identities.into_iter();
        let documents = serialize_routing_bus_duplication(
            &project,
            &assets,
            RoutingBusDuplicationSource {
                track: &return_track,
                mixer: &mixer,
                automation_take_ids: &[TAKE_ID],
                automation_takes: &[(TAKE_ID, take.as_slice())],
                device_chain_id: Some("018f47a2-6b2d-7c11-8a44-001122335017"),
                device_chain: Some(&return_chain),
            },
            RETURN_ID,
            "Verb copy",
            LoaderLimits::VERSION_1,
            || identities.next().unwrap().to_owned(),
        )
        .unwrap();

        assert!(documents.automation_takes().is_empty());
        let mixer_value: Value = serde_json::from_slice(documents.mixer().bytes()).unwrap();
        assert_eq!(mixer_value["channels"][0]["sends"].as_array().unwrap().len(), 1);
        assert_eq!(mixer_value["return_buses"], serde_json::json!([
            {"bus":"a", "track_id":RETURN_ID},
            {"bus":"b", "track_id":documents.track().id()}
        ]));
        let cloned_chain = DeviceChain::parse(documents.device_chain().unwrap().bytes()).unwrap();
        assert_eq!(cloned_chain.track_id(), documents.track().id());
    }

    #[test]
    fn refuses_return_duplication_when_all_return_buses_are_occupied() {
        let (timeline, track, return_track, clip, scene, arrangement, mixer, take, device_chain) = fixture();
        let second_return_id = "018f47a2-6b2d-7c11-8a44-001122335204";
        let mut second_return: Value = serde_json::from_slice(&return_track).unwrap();
        second_return["id"] = second_return_id.into();
        second_return["name"] = "Delay".into();
        second_return["order"] = 2.into();
        let second_return = canonical(second_return);
        let mut occupied_mixer: Value = serde_json::from_slice(&mixer).unwrap();
        occupied_mixer["channels"].as_array_mut().unwrap().push(serde_json::json!({
            "track_id": second_return_id, "gain_millionths": 1_000_000,
            "pan_millionths": 0, "mute": false, "solo": false,
            "destination": {"kind": "main"}, "sends": []
        }));
        occupied_mixer["return_buses"] = serde_json::json!([
            {"bus": "a", "track_id": RETURN_ID},
            {"bus": "b", "track_id": second_return_id}
        ]);
        let occupied_mixer = canonical(occupied_mixer);
        let original_mixer = occupied_mixer.clone();
        let mut return_chain: Value = serde_json::from_slice(&device_chain).unwrap();
        return_chain["track_id"] = RETURN_ID.into();
        let return_chain = canonical(return_chain);
        let tracks = [track.as_slice(), return_track.as_slice(), second_return.as_slice()];
        let clips = [clip.as_slice()];
        let scenes = [scene.as_slice()];
        let project = crate::load(
            crate::PlaybackJsonInput {
                project_id: PROJECT_ID,
                sample_rate_hz: 48_000,
                timeline: &timeline,
                tracks: &tracks,
                clips: &clips,
                scenes: &scenes,
                arrangement: Some(&arrangement),
            },
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let assets = [MediaAssetFact {
            id: ASSET_ID,
            sample_rate_hz: 48_000,
            frame_count: 24_000,
        }];
        let result = serialize_routing_bus_duplication(
            &project,
            &assets,
            RoutingBusDuplicationSource {
                track: &return_track,
                mixer: &occupied_mixer,
                automation_take_ids: &[TAKE_ID],
                automation_takes: &[(TAKE_ID, take.as_slice())],
                device_chain_id: Some("018f47a2-6b2d-7c11-8a44-001122335017"),
                device_chain: Some(&return_chain),
            },
            RETURN_ID,
            "Verb copy",
            LoaderLimits::VERSION_1,
            || "018f47a2-6b2d-7c11-8a44-001122335205".to_owned(),
        );

        assert!(matches!(
            result,
            Err(RoutingBusDuplicationSerializationError::ReturnBusUnavailable)
        ));
        assert_eq!(occupied_mixer, original_mixer);
    }

    #[test]
    fn removes_the_complete_audio_track_identity_graph_and_keeps_immutable_media() {
        let (timeline, track, return_track, clip, scene, arrangement, mixer, take, device_chain) = fixture();
        let tracks = [track.as_slice(), return_track.as_slice()];
        let clips = [clip.as_slice()];
        let scenes = [scene.as_slice()];
        let project = crate::load(
            crate::PlaybackJsonInput {
                project_id: PROJECT_ID,
                sample_rate_hz: 48_000,
                timeline: &timeline,
                tracks: &tracks,
                clips: &clips,
                scenes: &scenes,
                arrangement: Some(&arrangement),
            },
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let assets = [MediaAssetFact {
            id: ASSET_ID,
            sample_rate_hz: 48_000,
            frame_count: 24_000,
        }];
        let documents = serialize_audio_track_content_removal(
            &project,
            &assets,
            AudioTrackContentRemovalSource {
                track: &track,
                clips: &[(CLIP_ID, clip.as_slice())],
                scenes: &[(SCENE_ID, scene.as_slice())],
                arrangement: Some(&arrangement),
                mixer: &mixer,
                automation_take_ids: &[TAKE_ID],
                automation_takes: &[(TAKE_ID, take.as_slice())],
                device_chain_id: Some("018f47a2-6b2d-7c11-8a44-001122335017"),
                device_chain: Some(&device_chain),
            },
            TRACK_ID,
            LoaderLimits::VERSION_1,
        )
        .unwrap();

        assert_eq!(documents.track().id(), TRACK_ID);
        assert_eq!(documents.track().bytes(), track);
        assert_eq!(documents.clips().len(), 1);
        assert_eq!(documents.clips()[0].id(), CLIP_ID);
        assert_eq!(documents.clips()[0].bytes(), clip);
        assert_eq!(documents.device_chain().unwrap().bytes(), device_chain);
        assert_eq!(documents.scenes().len(), 1);
        assert_eq!(documents.automation_takes().len(), 1);

        let remaining_tracks = [return_track.as_slice()];
        let remaining_scenes = [documents.scenes()[0].bytes()];
        let reduced = crate::load(
            crate::PlaybackJsonInput {
                project_id: PROJECT_ID,
                sample_rate_hz: 48_000,
                timeline: &timeline,
                tracks: &remaining_tracks,
                clips: &[],
                scenes: &remaining_scenes,
                arrangement: documents.arrangement().map(AudioTrackContentRemovalDocument::bytes),
            },
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let reduced_mixer = reduced
            .load_mixer(documents.mixer().bytes(), &assets, LoaderLimits::VERSION_1)
            .unwrap();
        let automation = reduced
            .load_automation_takes(
                &reduced_mixer,
                &assets,
                &[documents.automation_takes()[0].bytes()],
                LoaderLimits::VERSION_1,
            )
            .unwrap();
        reduced
            .compile_main_automation(&reduced_mixer.compile_main_graph().unwrap(), &automation)
            .unwrap();
        assert_eq!(reduced.track_count(), 1);
        assert_eq!(reduced.clip_count(), 0);
        let scene_value: Value = serde_json::from_slice(documents.scenes()[0].bytes()).unwrap();
        assert_eq!(scene_value["slots"], serde_json::json!([]));
        let arrangement_value: Value =
            serde_json::from_slice(documents.arrangement().unwrap().bytes()).unwrap();
        assert_eq!(arrangement_value["placements"], serde_json::json!([]));
        let automation_value: Value =
            serde_json::from_slice(documents.automation_takes()[0].bytes()).unwrap();
        assert_eq!(automation_value["lanes"].as_array().unwrap().len(), 1);
        assert_eq!(automation_value["lanes"][0]["id"], MAIN_LANE_ID);
        assert_eq!(assets[0].id, ASSET_ID);
    }

    #[test]
    fn track_content_removal_refuses_incomplete_owned_document_authority() {
        let (timeline, track, return_track, clip, scene, arrangement, mixer, take, device_chain) = fixture();
        let tracks = [track.as_slice(), return_track.as_slice()];
        let clips = [clip.as_slice()];
        let scenes = [scene.as_slice()];
        let project = crate::load(
            crate::PlaybackJsonInput {
                project_id: PROJECT_ID,
                sample_rate_hz: 48_000,
                timeline: &timeline,
                tracks: &tracks,
                clips: &clips,
                scenes: &scenes,
                arrangement: Some(&arrangement),
            },
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let assets = [MediaAssetFact {
            id: ASSET_ID,
            sample_rate_hz: 48_000,
            frame_count: 24_000,
        }];
        let source = AudioTrackContentRemovalSource {
            track: &track,
            clips: &[],
            scenes: &[(SCENE_ID, scene.as_slice())],
            arrangement: Some(&arrangement),
            mixer: &mixer,
            automation_take_ids: &[TAKE_ID],
            automation_takes: &[(TAKE_ID, take.as_slice())],
            device_chain_id: Some("018f47a2-6b2d-7c11-8a44-001122335017"),
            device_chain: Some(&device_chain),
        };
        assert!(matches!(
            serialize_audio_track_content_removal(
                &project,
                &assets,
                source,
                TRACK_ID,
                LoaderLimits::VERSION_1,
            ),
            Err(AudioTrackContentRemovalSerializationError::SourceClipCoverage)
        ));
    }

    #[test]
    fn track_content_removal_refuses_unknown_owned_track_extensions() {
        let (timeline, track, return_track, clip, scene, arrangement, mixer, take, device_chain) = fixture();
        let mut extended_track: Value = serde_json::from_slice(&track).unwrap();
        extended_track["extensions"] = serde_json::json!({"org.example.owned-state": {"opaque": true}});
        let extended_track = canonical(extended_track);
        let tracks = [extended_track.as_slice(), return_track.as_slice()];
        let clips = [clip.as_slice()];
        let scenes = [scene.as_slice()];
        let project = crate::load(
            crate::PlaybackJsonInput {
                project_id: PROJECT_ID,
                sample_rate_hz: 48_000,
                timeline: &timeline,
                tracks: &tracks,
                clips: &clips,
                scenes: &scenes,
                arrangement: Some(&arrangement),
            },
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let assets = [MediaAssetFact {
            id: ASSET_ID,
            sample_rate_hz: 48_000,
            frame_count: 24_000,
        }];
        assert!(matches!(
            serialize_audio_track_content_removal(
                &project,
                &assets,
                AudioTrackContentRemovalSource {
                    track: &extended_track,
                    clips: &[(CLIP_ID, clip.as_slice())],
                    scenes: &[(SCENE_ID, scene.as_slice())],
                    arrangement: Some(&arrangement),
                    mixer: &mixer,
                    automation_take_ids: &[TAKE_ID],
                    automation_takes: &[(TAKE_ID, take.as_slice())],
                    device_chain_id: Some("018f47a2-6b2d-7c11-8a44-001122335017"),
                    device_chain: Some(&device_chain),
                },
                TRACK_ID,
                LoaderLimits::VERSION_1,
            ),
            Err(AudioTrackContentRemovalSerializationError::UnknownExtensionSemantics)
        ));
    }

    #[test]
    fn refuses_unknown_extension_copy_semantics_before_generating_output() {
        let (timeline, track, return_track, clip, scene, arrangement, mixer, take, _) = fixture();
        let tracks = [track.as_slice(), return_track.as_slice()];
        let clips = [clip.as_slice()];
        let scenes = [scene.as_slice()];
        let project = crate::load(
            crate::PlaybackJsonInput {
                project_id: PROJECT_ID,
                sample_rate_hz: 48_000,
                timeline: &timeline,
                tracks: &tracks,
                clips: &clips,
                scenes: &scenes,
                arrangement: Some(&arrangement),
            },
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let mut extended_track: Value = serde_json::from_slice(&track).unwrap();
        extended_track["extensions"] = serde_json::json!({"org.example.identity": {"opaque": true}});
        let extended_track = canonical(extended_track);
        let assets = [MediaAssetFact {
            id: ASSET_ID,
            sample_rate_hz: 48_000,
            frame_count: 24_000,
        }];
        assert!(matches!(
            serialize_audio_track_duplication(
                &project,
                &assets,
                AudioTrackDuplicationSource {
                    track: &extended_track,
                    clips: &[(CLIP_ID, clip.as_slice())],
                    scenes: &[(SCENE_ID, scene.as_slice())],
                    arrangement: Some(&arrangement),
                    mixer: &mixer,
                    automation_take_ids: &[TAKE_ID],
                    automation_takes: &[(TAKE_ID, take.as_slice())],
                    device_chain_id: None,
                    device_chain: None,
                },
                TRACK_ID,
                "Lead copy",
                LoaderLimits::VERSION_1,
                || "018f47a2-6b2d-7c11-8a44-001122335101".to_owned(),
            ),
            Err(AudioTrackDuplicationSerializationError::UnknownExtensionSemantics)
        ));
        assert_eq!(track, fixture().1);
    }
}
