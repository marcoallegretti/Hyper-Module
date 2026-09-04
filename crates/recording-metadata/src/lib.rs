#![doc = "Validated metadata for verified recording publications."]

use hyper_recording_journal::PublishedRecording;
use hyper_strict_json::{StrictJsonError, StrictJsonErrorKind};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

const FORMAT: &str = "hyper-recording-metadata";
const FORMAT_VERSION: u8 = 1;
const HANDOFF_FORMAT: &str = "hyper-finalized-recording-handoff";
const HANDOFF_FORMAT_VERSION: u8 = 1;
const VERSION_ONE_SAMPLE_RATE: u32 = 48_000;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_METADATA_BYTES: usize = 1_048_576;
const TAKE_LABEL_EXTENSION: &str = "io.hypermodule.recording-take";
const TAKE_LABEL_FIELD: &str = "label";
const MAX_TAKE_LABEL_CHARACTERS: usize = 120;

/// Maximum byte size accepted for one finalized-recording handoff.
///
/// The complete metadata object is independently bounded to one MiB. The
/// two-MiB handoff boundary retains the established application transport
/// limit while rejecting unrelated file contents before deserialization.
pub const MAX_FINALIZED_RECORDING_HANDOFF_BYTES: usize = 2 * MAX_METADATA_BYTES;

/// Maximum UTF-8 length of the absolute published WAV path in one handoff.
pub const MAX_FINALIZED_RECORDING_HANDOFF_PATH_UTF8_BYTES: usize = 4_096;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordingCaptureKind {
    New,
    Overdub,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RecordingMetadataDocument {
    id: String,
    project_id: String,
    asset_id: String,
    track_id: String,
    capture_kind: RecordingCaptureKind,
    captured_at_utc: String,
    timeline_start_tick: u64,
    source_input_indices: Vec<u8>,
    sample_rate_hz: u32,
    channels: u8,
    frame_count: u64,
    extensions: BTreeMap<String, serde_json::Value>,
}

impl RecordingMetadataDocument {
    /// Parses and validates one complete version-1 recording metadata
    /// document.
    ///
    /// Filesystem canonicalization and descriptor integrity remain the storage
    /// layer's responsibility.
    ///
    /// # Errors
    ///
    /// Returns a strict JSON or recording-domain rejection.
    pub fn parse(bytes: &[u8]) -> Result<Self, RecordingMetadataParseError> {
        let raw: RawRecordingMetadata = hyper_strict_json::parse(bytes, MAX_METADATA_BYTES)?;
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
    pub fn asset_id(&self) -> &str {
        &self.asset_id
    }

    #[must_use]
    pub fn track_id(&self) -> &str {
        &self.track_id
    }

    #[must_use]
    pub const fn capture_kind(&self) -> RecordingCaptureKind {
        self.capture_kind
    }

    #[must_use]
    pub fn captured_at_utc(&self) -> &str {
        &self.captured_at_utc
    }

    #[must_use]
    pub const fn timeline_start_tick(&self) -> u64 {
        self.timeline_start_tick
    }

    #[must_use]
    pub fn source_input_indices(&self) -> &[u8] {
        &self.source_input_indices
    }

    #[must_use]
    pub const fn sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz
    }

    #[must_use]
    pub const fn channels(&self) -> u8 {
        self.channels
    }

    #[must_use]
    pub const fn frame_count(&self) -> u64 {
        self.frame_count
    }

    #[must_use]
    pub const fn extensions(&self) -> &BTreeMap<String, serde_json::Value> {
        &self.extensions
    }

    /// Returns the user-assigned label for this recorded take, if present.
    #[must_use]
    pub fn take_label(&self) -> Option<&str> {
        take_label_from_extensions(&self.extensions)
    }

    /// Rewrites this metadata document with one validated user-facing take
    /// label while preserving every unrelated extension value.
    ///
    /// # Errors
    ///
    /// Returns an error if `label` is not a trimmed 1 through 120 character
    /// string or canonical JSON serialization fails.
    pub fn with_take_label(&self, label: &str) -> Result<Vec<u8>, RecordingMetadataEditError> {
        if !valid_take_label(label) {
            return Err(RecordingMetadataEditError::InvalidTakeLabel);
        }
        let mut extensions = self.extensions.clone();
        let mut take_extension = extensions
            .get(TAKE_LABEL_EXTENSION)
            .and_then(serde_json::Value::as_object)
            .cloned()
            .unwrap_or_default();
        take_extension.insert(
            TAKE_LABEL_FIELD.to_owned(),
            serde_json::Value::String(label.to_owned()),
        );
        extensions.insert(
            TAKE_LABEL_EXTENSION.to_owned(),
            serde_json::Value::Object(take_extension),
        );
        serde_json_canonicalizer::to_vec(&CanonicalDocument {
            asset_id: &self.asset_id,
            capture_kind: self.capture_kind,
            captured_at_utc: &self.captured_at_utc,
            channels: self.channels,
            extensions,
            format: FORMAT,
            format_version: FORMAT_VERSION,
            frame_count: self.frame_count,
            id: &self.id,
            project_id: &self.project_id,
            sample_rate_hz: self.sample_rate_hz,
            source_input_indices: &self.source_input_indices,
            timeline_start_tick: self.timeline_start_tick,
            track_id: &self.track_id,
        })
        .map_err(RecordingMetadataEditError::Serialization)
    }
}

/// A fully validated, canonical finalized-recording handoff.
///
/// This type deliberately validates only the transport descriptor and
/// recording metadata. It does not open or trust the referenced WAV file;
/// filesystem identity, media integrity, destination selection, and durable
/// import remain responsibilities of the receiving storage transaction.
#[derive(Clone, Debug, PartialEq)]
pub struct FinalizedRecordingHandoff {
    canonical_bytes: Vec<u8>,
    recording_path: PathBuf,
    recording_byte_length: u64,
    recording_sha256: [u8; 32],
    metadata: RecordingMetadataDocument,
    metadata_bytes: Vec<u8>,
}

impl FinalizedRecordingHandoff {
    /// Parses one complete canonical version-1 finalized-recording handoff.
    ///
    /// # Errors
    ///
    /// Refuses oversized, malformed, noncanonical, or incompatible handoffs
    /// before callers can use the media descriptor or metadata facts.
    pub fn parse(bytes: &[u8]) -> Result<Self, FinalizedRecordingHandoffParseError> {
        if bytes.len() > MAX_FINALIZED_RECORDING_HANDOFF_BYTES {
            return Err(FinalizedRecordingHandoffParseError::limit(
                "recording handoff exceeds the version-one byte capacity",
            ));
        }
        let value: serde_json::Value =
            hyper_strict_json::parse(bytes, MAX_FINALIZED_RECORDING_HANDOFF_BYTES).map_err(
                |source| {
                    FinalizedRecordingHandoffParseError::with_source(
                        FinalizedRecordingHandoffParseErrorKind::Json(source.kind),
                        "recording handoff is not strict JSON",
                        source,
                    )
                },
            )?;
        let canonical = serde_json_canonicalizer::to_vec(&value).map_err(|source| {
            FinalizedRecordingHandoffParseError::with_source(
                FinalizedRecordingHandoffParseErrorKind::InvalidDomain,
                "recording handoff cannot be canonicalized",
                source,
            )
        })?;
        if canonical != bytes {
            return Err(FinalizedRecordingHandoffParseError::noncanonical());
        }
        let raw: RawFinalizedRecordingHandoff =
            serde_json::from_value(value).map_err(|source| {
                FinalizedRecordingHandoffParseError::with_source(
                    FinalizedRecordingHandoffParseErrorKind::InvalidDomain,
                    "recording handoff shape is invalid",
                    source,
                )
            })?;
        if raw.format != HANDOFF_FORMAT
            || raw.format_version != HANDOFF_FORMAT_VERSION
            || !raw.extensions.is_empty()
            || raw.recording.media_type != "audio/wav"
            || raw.recording.path.is_empty()
        {
            return Err(FinalizedRecordingHandoffParseError::invalid(
                "recording handoff domain is invalid",
            ));
        }
        let recording_path = PathBuf::from(raw.recording.path);
        handoff_publication_path(&recording_path).map_err(|source| {
            FinalizedRecordingHandoffParseError::with_source(
                FinalizedRecordingHandoffParseErrorKind::InvalidDomain,
                "recording handoff publication path is invalid",
                source,
            )
        })?;
        let recording_sha256 = decode_sha256(&raw.recording.sha256)?;
        let metadata_bytes = serde_json_canonicalizer::to_vec(&raw.metadata).map_err(|source| {
            FinalizedRecordingHandoffParseError::with_source(
                FinalizedRecordingHandoffParseErrorKind::InvalidDomain,
                "recording handoff metadata cannot be canonicalized",
                source,
            )
        })?;
        let metadata = RecordingMetadataDocument::parse(&metadata_bytes).map_err(|source| {
            FinalizedRecordingHandoffParseError::with_source(
                FinalizedRecordingHandoffParseErrorKind::Metadata(source.kind()),
                "recording handoff metadata is invalid",
                source,
            )
        })?;
        Ok(Self {
            canonical_bytes: bytes.to_vec(),
            recording_path,
            recording_byte_length: raw.recording.byte_length,
            recording_sha256,
            metadata,
            metadata_bytes,
        })
    }

    /// Returns the original canonical handoff bytes.
    #[must_use]
    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical_bytes
    }

    /// Returns the absolute source WAV path carried by the handoff.
    #[must_use]
    pub fn recording_path(&self) -> &Path {
        &self.recording_path
    }

    /// Returns the verified source WAV byte length carried by the handoff.
    #[must_use]
    pub const fn recording_byte_length(&self) -> u64 {
        self.recording_byte_length
    }

    /// Returns the SHA-256 descriptor carried by the handoff.
    #[must_use]
    pub const fn recording_sha256(&self) -> &[u8; 32] {
        &self.recording_sha256
    }

    /// Returns the validated recording metadata document.
    #[must_use]
    pub const fn metadata(&self) -> &RecordingMetadataDocument {
        &self.metadata
    }

    /// Returns canonical bytes for the embedded recording metadata document.
    #[must_use]
    pub fn metadata_bytes(&self) -> &[u8] {
        &self.metadata_bytes
    }
}

/// Classifies a finalized-recording handoff rejection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FinalizedRecordingHandoffParseErrorKind {
    /// The input was not valid strict JSON.
    Json(StrictJsonErrorKind),
    /// The input was valid JSON but not RFC 8785 canonical bytes.
    NonCanonicalJson,
    /// The handoff shape or version-one domain was invalid.
    InvalidDomain,
    /// Embedded recording metadata was rejected by its typed parser.
    Metadata(RecordingMetadataParseErrorKind),
    /// The handoff exceeded its fixed byte capacity.
    LimitExceeded,
}

/// A classified finalized-recording handoff parse failure.
#[derive(Debug)]
pub struct FinalizedRecordingHandoffParseError {
    kind: FinalizedRecordingHandoffParseErrorKind,
    message: String,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl FinalizedRecordingHandoffParseError {
    /// Returns the stable error category.
    #[must_use]
    pub const fn kind(&self) -> FinalizedRecordingHandoffParseErrorKind {
        self.kind
    }

    fn invalid(message: &'static str) -> Self {
        Self::new(
            FinalizedRecordingHandoffParseErrorKind::InvalidDomain,
            message,
        )
    }

    fn limit(message: &'static str) -> Self {
        Self::new(
            FinalizedRecordingHandoffParseErrorKind::LimitExceeded,
            message,
        )
    }

    fn noncanonical() -> Self {
        Self::new(
            FinalizedRecordingHandoffParseErrorKind::NonCanonicalJson,
            "recording handoff is not canonical RFC 8785 JSON",
        )
    }

    fn new(kind: FinalizedRecordingHandoffParseErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    fn with_source(
        kind: FinalizedRecordingHandoffParseErrorKind,
        message: impl Into<String>,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }
}

impl fmt::Display for FinalizedRecordingHandoffParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for FinalizedRecordingHandoffParseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_deref().map(|source| source as _)
    }
}

#[derive(Debug)]
pub enum RecordingMetadataEditError {
    InvalidTakeLabel,
    Serialization(serde_json::Error),
}

impl fmt::Display for RecordingMetadataEditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTakeLabel => {
                formatter.write_str("recorded-take label must contain 1 to 120 trimmed characters")
            }
            Self::Serialization(source) => {
                write!(
                    formatter,
                    "recording metadata cannot be canonicalized: {source}"
                )
            }
        }
    }
}

impl Error for RecordingMetadataEditError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Serialization(source) => Some(source),
            Self::InvalidTakeLabel => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingMetadataParseErrorKind {
    Json(StrictJsonErrorKind),
    InvalidDomain,
}

#[derive(Debug)]
pub struct RecordingMetadataParseError {
    kind: RecordingMetadataParseErrorKind,
    message: String,
    source: Option<StrictJsonError>,
}

impl RecordingMetadataParseError {
    #[must_use]
    pub const fn kind(&self) -> RecordingMetadataParseErrorKind {
        self.kind
    }
}

impl fmt::Display for RecordingMetadataParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for RecordingMetadataParseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_ref().map(|source| source as _)
    }
}

impl From<StrictJsonError> for RecordingMetadataParseError {
    fn from(error: StrictJsonError) -> Self {
        Self {
            kind: RecordingMetadataParseErrorKind::Json(error.kind),
            message: error.message.clone(),
            source: Some(error),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRecordingMetadata {
    format: String,
    format_version: u8,
    id: String,
    project_id: String,
    asset_id: String,
    track_id: String,
    capture_kind: RecordingCaptureKind,
    captured_at_utc: String,
    timeline_start_tick: u64,
    source_input_indices: Vec<u8>,
    sample_rate_hz: u32,
    channels: u8,
    frame_count: u64,
    extensions: BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFinalizedRecordingHandoff {
    format: String,
    format_version: u8,
    recording: RawFinalizedRecording,
    metadata: serde_json::Value,
    extensions: BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFinalizedRecording {
    path: String,
    media_type: String,
    byte_length: u64,
    sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordingMetadataContext {
    pub id: String,
    pub project_id: String,
    pub asset_id: String,
    pub track_id: String,
    pub capture_kind: RecordingCaptureKind,
    pub captured_at_utc: String,
    pub timeline_start_tick: u64,
    pub source_input_indices: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingMetadataError {
    InvalidId(&'static str),
    InvalidCapturedAt,
    TimelineStartOutOfRange,
    UnsupportedSampleRate { actual: u32 },
    InvalidSourceInputCount { actual: usize, expected: usize },
    InvalidSourceInput { index: u8 },
    DuplicateSourceInput { index: u8 },
}

impl fmt::Display for RecordingMetadataError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidId(field) => write!(formatter, "{field} is not a canonical entity ID"),
            Self::InvalidCapturedAt => {
                formatter.write_str("captured_at_utc is not a canonical UTC instant")
            }
            Self::TimelineStartOutOfRange => {
                formatter.write_str("timeline_start_tick exceeds the interoperable integer range")
            }
            Self::UnsupportedSampleRate { actual } => write!(
                formatter,
                "recording sample rate is {actual} Hz; version 1 requires 48000 Hz"
            ),
            Self::InvalidSourceInputCount { actual, expected } => write!(
                formatter,
                "recording has {expected} channels but {actual} source input indices"
            ),
            Self::InvalidSourceInput { index } => {
                write!(
                    formatter,
                    "source input index {index} is unavailable in version 1"
                )
            }
            Self::DuplicateSourceInput { index } => {
                write!(formatter, "source input index {index} is duplicated")
            }
        }
    }
}

impl Error for RecordingMetadataError {}

#[derive(Debug)]
pub struct RecordingMetadataBuildError {
    reason: RecordingMetadataError,
    rejected: Box<RejectedMetadata>,
}

#[derive(Debug)]
struct RejectedMetadata {
    publication: PublishedRecording,
    context: RecordingMetadataContext,
}

impl RecordingMetadataBuildError {
    #[must_use]
    pub const fn reason(&self) -> RecordingMetadataError {
        self.reason
    }

    #[must_use]
    pub const fn publication(&self) -> &PublishedRecording {
        &self.rejected.publication
    }

    #[must_use]
    pub const fn context(&self) -> &RecordingMetadataContext {
        &self.rejected.context
    }

    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        RecordingMetadataError,
        PublishedRecording,
        RecordingMetadataContext,
    ) {
        let RejectedMetadata {
            publication,
            context,
        } = *self.rejected;
        (self.reason, publication, context)
    }
}

impl fmt::Display for RecordingMetadataBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.reason.fmt(formatter)
    }
}

impl Error for RecordingMetadataBuildError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.reason)
    }
}

#[derive(Debug)]
pub enum RecordingHandoffError {
    NonAbsolutePublicationPath,
    NonUnicodePublicationPath,
    PublicationPathContainsNul,
    PublicationPathTooLong { actual_bytes: usize, maximum: usize },
    Serialization(serde_json::Error),
}

impl fmt::Display for RecordingHandoffError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonAbsolutePublicationPath => {
                formatter.write_str("recording publication path must be absolute")
            }
            Self::NonUnicodePublicationPath => {
                formatter.write_str("recording publication path is not valid Unicode")
            }
            Self::PublicationPathContainsNul => {
                formatter.write_str("recording publication path contains NUL")
            }
            Self::PublicationPathTooLong {
                actual_bytes,
                maximum,
            } => write!(
                formatter,
                "recording publication path has {actual_bytes} UTF-8 bytes; maximum is {maximum}"
            ),
            Self::Serialization(source) => {
                write!(
                    formatter,
                    "recording handoff serialization failed: {source}"
                )
            }
        }
    }
}

impl Error for RecordingHandoffError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Serialization(source) => Some(source),
            Self::NonAbsolutePublicationPath
            | Self::NonUnicodePublicationPath
            | Self::PublicationPathContainsNul
            | Self::PublicationPathTooLong { .. } => None,
        }
    }
}

#[derive(Debug)]
pub struct FinalizedRecordingMetadata {
    publication: PublishedRecording,
    context: RecordingMetadataContext,
}

impl FinalizedRecordingMetadata {
    /// Consumes verified publication authority and validates version-1
    /// recording provenance.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid identities, capture time, timeline range,
    /// media rate, or physical input mapping.
    pub fn try_new(
        publication: PublishedRecording,
        context: RecordingMetadataContext,
    ) -> Result<Self, RecordingMetadataBuildError> {
        if let Err(reason) = validate(&publication, &context) {
            return Err(RecordingMetadataBuildError {
                reason,
                rejected: Box::new(RejectedMetadata {
                    publication,
                    context,
                }),
            });
        }
        Ok(Self {
            publication,
            context,
        })
    }

    #[must_use]
    pub const fn publication(&self) -> &PublishedRecording {
        &self.publication
    }

    #[must_use]
    pub const fn context(&self) -> &RecordingMetadataContext {
        &self.context
    }

    /// Serializes canonical version-1 JSON with lexicographically ordered
    /// object members and an empty extensions object.
    ///
    /// # Errors
    ///
    /// Returns a serialization error if JSON encoding fails.
    pub fn to_canonical_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(&self.canonical_metadata())
    }

    /// Serializes the verified publication and metadata as one canonical
    /// application handoff document.
    ///
    /// The destination project is deliberately absent and remains owned by
    /// the receiving application.
    ///
    /// # Errors
    ///
    /// Returns an error when the publication path is not absolute, cannot be
    /// represented losslessly as Unicode, contains NUL, exceeds 4096 UTF-8
    /// bytes, or JSON serialization fails.
    pub fn to_handoff_json(&self) -> Result<Vec<u8>, RecordingHandoffError> {
        let path = handoff_publication_path(self.publication.final_wav_path())?;
        serde_json::to_vec(&CanonicalHandoff {
            extensions: BTreeMap::new(),
            format: HANDOFF_FORMAT,
            format_version: HANDOFF_FORMAT_VERSION,
            metadata: self.canonical_metadata(),
            recording: CanonicalRecording {
                byte_length: self.publication.byte_length(),
                media_type: "audio/wav",
                path,
                sha256: hex_sha256(self.publication.wav_sha256()),
            },
        })
        .map_err(RecordingHandoffError::Serialization)
    }

    fn canonical_metadata(&self) -> CanonicalMetadata<'_> {
        CanonicalMetadata {
            asset_id: &self.context.asset_id,
            capture_kind: self.context.capture_kind,
            captured_at_utc: &self.context.captured_at_utc,
            channels: self.publication.channels(),
            extensions: BTreeMap::new(),
            format: FORMAT,
            format_version: FORMAT_VERSION,
            frame_count: self.publication.frames(),
            id: &self.context.id,
            project_id: &self.context.project_id,
            sample_rate_hz: self.publication.sample_rate_hz(),
            source_input_indices: &self.context.source_input_indices,
            timeline_start_tick: self.context.timeline_start_tick,
            track_id: &self.context.track_id,
        }
    }
}

fn handoff_publication_path(path: &Path) -> Result<&str, RecordingHandoffError> {
    if !path.is_absolute() {
        return Err(RecordingHandoffError::NonAbsolutePublicationPath);
    }
    let path = path
        .to_str()
        .ok_or(RecordingHandoffError::NonUnicodePublicationPath)?;
    if path.contains('\0') {
        return Err(RecordingHandoffError::PublicationPathContainsNul);
    }
    let actual_bytes = path.len();
    if actual_bytes > MAX_FINALIZED_RECORDING_HANDOFF_PATH_UTF8_BYTES {
        return Err(RecordingHandoffError::PublicationPathTooLong {
            actual_bytes,
            maximum: MAX_FINALIZED_RECORDING_HANDOFF_PATH_UTF8_BYTES,
        });
    }
    Ok(path)
}

fn decode_sha256(value: &str) -> Result<[u8; 32], FinalizedRecordingHandoffParseError> {
    if !valid_sha256(value) {
        return Err(FinalizedRecordingHandoffParseError::invalid(
            "recording handoff SHA-256 digest is invalid",
        ));
    }
    let mut output = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_nibble(pair[0]).ok_or_else(|| {
            FinalizedRecordingHandoffParseError::invalid(
                "recording handoff SHA-256 digest is invalid",
            )
        })?;
        let low = hex_nibble(pair[1]).ok_or_else(|| {
            FinalizedRecordingHandoffParseError::invalid(
                "recording handoff SHA-256 digest is invalid",
            )
        })?;
        output[index] = (high << 4) | low;
    }
    Ok(output)
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn validate_document(
    raw: RawRecordingMetadata,
) -> Result<RecordingMetadataDocument, RecordingMetadataParseError> {
    require_document(raw.format == FORMAT, "recording metadata format is invalid")?;
    require_document(
        raw.format_version == FORMAT_VERSION,
        "recording metadata format version is unsupported",
    )?;
    require_document(canonical_id(&raw.id), "recording metadata ID is invalid")?;
    require_document(
        canonical_id(&raw.project_id),
        "recording metadata project ID is invalid",
    )?;
    require_document(
        canonical_id(&raw.asset_id),
        "recording metadata asset ID is invalid",
    )?;
    require_document(
        canonical_id(&raw.track_id),
        "recording metadata track ID is invalid",
    )?;
    require_document(
        canonical_utc(&raw.captured_at_utc),
        "recording capture time is invalid",
    )?;
    require_document(
        raw.timeline_start_tick <= MAX_SAFE_INTEGER,
        "recording timeline start exceeds the interoperable integer range",
    )?;
    require_document(
        raw.sample_rate_hz == VERSION_ONE_SAMPLE_RATE,
        "recording sample rate is unsupported",
    )?;
    require_document(
        (1..=2).contains(&raw.channels),
        "recording channel count is invalid",
    )?;
    require_document(
        raw.frame_count > 0 && raw.frame_count <= MAX_SAFE_INTEGER,
        "recording frame count is invalid",
    )?;
    require_document(
        raw.source_input_indices.len() == usize::from(raw.channels),
        "recording source input count does not match its channels",
    )?;
    let mut seen = [false; 2];
    for &index in &raw.source_input_indices {
        let Some(slot) = seen.get_mut(usize::from(index)) else {
            return Err(document_error("recording source input is invalid"));
        };
        require_document(!*slot, "recording source inputs are not unique")?;
        *slot = true;
    }
    require_document(
        raw.extensions.keys().all(|key| valid_extension_key(key)),
        "recording metadata extensions contain an invalid key",
    )?;
    validate_take_label_extension(&raw.extensions)?;

    Ok(RecordingMetadataDocument {
        id: raw.id,
        project_id: raw.project_id,
        asset_id: raw.asset_id,
        track_id: raw.track_id,
        capture_kind: raw.capture_kind,
        captured_at_utc: raw.captured_at_utc,
        timeline_start_tick: raw.timeline_start_tick,
        source_input_indices: raw.source_input_indices,
        sample_rate_hz: raw.sample_rate_hz,
        channels: raw.channels,
        frame_count: raw.frame_count,
        extensions: raw.extensions,
    })
}

fn take_label_from_extensions(extensions: &BTreeMap<String, serde_json::Value>) -> Option<&str> {
    extensions
        .get(TAKE_LABEL_EXTENSION)
        .and_then(serde_json::Value::as_object)
        .and_then(|extension| extension.get(TAKE_LABEL_FIELD))
        .and_then(serde_json::Value::as_str)
}

fn validate_take_label_extension(
    extensions: &BTreeMap<String, serde_json::Value>,
) -> Result<(), RecordingMetadataParseError> {
    let Some(extension) = extensions.get(TAKE_LABEL_EXTENSION) else {
        return Ok(());
    };
    let label = extension
        .as_object()
        .and_then(|extension| extension.get(TAKE_LABEL_FIELD))
        .and_then(serde_json::Value::as_str);
    require_document(
        label.is_some_and(valid_take_label),
        "recording metadata take label extension is invalid",
    )
}

fn valid_take_label(value: &str) -> bool {
    let count = value.chars().count();
    value.trim() == value && (1..=MAX_TAKE_LABEL_CHARACTERS).contains(&count)
}

fn require_document(
    condition: bool,
    message: &'static str,
) -> Result<(), RecordingMetadataParseError> {
    if condition {
        Ok(())
    } else {
        Err(document_error(message))
    }
}

fn document_error(message: &'static str) -> RecordingMetadataParseError {
    RecordingMetadataParseError {
        kind: RecordingMetadataParseErrorKind::InvalidDomain,
        message: message.to_owned(),
        source: None,
    }
}

fn validate(
    publication: &PublishedRecording,
    context: &RecordingMetadataContext,
) -> Result<(), RecordingMetadataError> {
    validate_id("id", &context.id)?;
    validate_id("project_id", &context.project_id)?;
    validate_id("asset_id", &context.asset_id)?;
    validate_id("track_id", &context.track_id)?;
    if !canonical_utc(&context.captured_at_utc) {
        return Err(RecordingMetadataError::InvalidCapturedAt);
    }
    if context.timeline_start_tick > MAX_SAFE_INTEGER {
        return Err(RecordingMetadataError::TimelineStartOutOfRange);
    }
    if publication.sample_rate_hz() != VERSION_ONE_SAMPLE_RATE {
        return Err(RecordingMetadataError::UnsupportedSampleRate {
            actual: publication.sample_rate_hz(),
        });
    }
    let expected = usize::from(publication.channels());
    if context.source_input_indices.len() != expected {
        return Err(RecordingMetadataError::InvalidSourceInputCount {
            actual: context.source_input_indices.len(),
            expected,
        });
    }
    let mut seen = [false; 2];
    for &index in &context.source_input_indices {
        let slot = seen
            .get_mut(usize::from(index))
            .ok_or(RecordingMetadataError::InvalidSourceInput { index })?;
        if *slot {
            return Err(RecordingMetadataError::DuplicateSourceInput { index });
        }
        *slot = true;
    }
    Ok(())
}

#[derive(Serialize)]
struct CanonicalMetadata<'a> {
    asset_id: &'a str,
    capture_kind: RecordingCaptureKind,
    captured_at_utc: &'a str,
    channels: u16,
    extensions: BTreeMap<String, serde_json::Value>,
    format: &'static str,
    format_version: u8,
    frame_count: u64,
    id: &'a str,
    project_id: &'a str,
    sample_rate_hz: u32,
    source_input_indices: &'a [u8],
    timeline_start_tick: u64,
    track_id: &'a str,
}

#[derive(Serialize)]
struct CanonicalDocument<'a> {
    asset_id: &'a str,
    capture_kind: RecordingCaptureKind,
    captured_at_utc: &'a str,
    channels: u8,
    extensions: BTreeMap<String, serde_json::Value>,
    format: &'static str,
    format_version: u8,
    frame_count: u64,
    id: &'a str,
    project_id: &'a str,
    sample_rate_hz: u32,
    source_input_indices: &'a [u8],
    timeline_start_tick: u64,
    track_id: &'a str,
}

#[derive(Serialize)]
struct CanonicalHandoff<'a> {
    extensions: BTreeMap<String, serde_json::Value>,
    format: &'static str,
    format_version: u8,
    metadata: CanonicalMetadata<'a>,
    recording: CanonicalRecording<'a>,
}

#[derive(Serialize)]
struct CanonicalRecording<'a> {
    byte_length: u64,
    media_type: &'static str,
    path: &'a str,
    sha256: String,
}

fn hex_sha256(digest: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(64);
    for byte in digest {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn validate_id(field: &'static str, value: &str) -> Result<(), RecordingMetadataError> {
    if canonical_id(value) {
        Ok(())
    } else {
        Err(RecordingMetadataError::InvalidId(field))
    }
}

fn canonical_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(byte),
        })
}

fn valid_extension_key(value: &str) -> bool {
    let mut labels = 0_usize;
    for label in value.split('.') {
        labels += 1;
        let bytes = label.as_bytes();
        if bytes.is_empty()
            || !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit()
            || !bytes[bytes.len() - 1].is_ascii_lowercase()
                && !bytes[bytes.len() - 1].is_ascii_digit()
            || !bytes
                .iter()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
        {
            return false;
        }
    }
    labels >= 2
}

fn canonical_utc(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 24
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'.'
        || bytes[23] != b'Z'
    {
        return false;
    }
    let Some(year) = decimal(bytes, 0, 4) else {
        return false;
    };
    let Some(month) = decimal(bytes, 5, 2) else {
        return false;
    };
    let Some(day) = decimal(bytes, 8, 2) else {
        return false;
    };
    let Some(hour) = decimal(bytes, 11, 2) else {
        return false;
    };
    let Some(minute) = decimal(bytes, 14, 2) else {
        return false;
    };
    let Some(second) = decimal(bytes, 17, 2) else {
        return false;
    };
    if decimal(bytes, 20, 3).is_none() || !(1..=12).contains(&month) {
        return false;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    (1..=days).contains(&day) && hour < 24 && minute < 60 && second < 60
}

fn decimal(bytes: &[u8], start: usize, length: usize) -> Option<u32> {
    bytes
        .get(start..start + length)?
        .iter()
        .try_fold(0_u32, |value, byte| {
            if byte.is_ascii_digit() {
                Some(value * 10 + u32::from(byte - b'0'))
            } else {
                None
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    const ABSOLUTE_PREFIX: &str = "/";
    #[cfg(windows)]
    const ABSOLUTE_PREFIX: &str = "C:\\";

    fn recording_metadata_with_extensions(extensions: &serde_json::Value) -> Vec<u8> {
        serde_json_canonicalizer::to_vec(&serde_json::json!({
            "format": FORMAT,
            "format_version": FORMAT_VERSION,
            "id": "018f47a2-6b2d-7c11-8a44-001122334456",
            "project_id": "018f47a2-6b2d-7c11-8a44-001122334457",
            "asset_id": "018f47a2-6b2d-7c11-8a44-001122334458",
            "track_id": "018f47a2-6b2d-7c11-8a44-001122334459",
            "capture_kind": "new",
            "captured_at_utc": "2026-08-26T12:00:00.000Z",
            "timeline_start_tick": 0,
            "source_input_indices": [0],
            "sample_rate_hz": VERSION_ONE_SAMPLE_RATE,
            "channels": 1,
            "frame_count": 48_000,
            "extensions": extensions,
        }))
        .unwrap()
    }

    fn finalized_handoff() -> Vec<u8> {
        let metadata: serde_json::Value =
            serde_json::from_slice(&recording_metadata_with_extensions(&serde_json::json!({})))
                .unwrap();
        serde_json_canonicalizer::to_vec(&serde_json::json!({
            "extensions": {},
            "format": HANDOFF_FORMAT,
            "format_version": HANDOFF_FORMAT_VERSION,
            "metadata": metadata,
            "recording": {
                "byte_length": 96,
                "media_type": "audio/wav",
                "path": format!("{ABSOLUTE_PREFIX}recorded-take.wav"),
                "sha256": "a".repeat(64),
            },
        }))
        .unwrap()
    }

    #[test]
    fn finalized_handoff_parser_retains_one_complete_validated_contract() {
        let bytes = finalized_handoff();
        let handoff = FinalizedRecordingHandoff::parse(&bytes).unwrap();

        assert_eq!(handoff.canonical_bytes(), bytes);
        assert_eq!(
            handoff.recording_path(),
            Path::new(&format!("{ABSOLUTE_PREFIX}recorded-take.wav"))
        );
        assert_eq!(handoff.recording_byte_length(), 96);
        assert_eq!(handoff.recording_sha256(), &[0xaa; 32]);
        assert_eq!(
            handoff.metadata().project_id(),
            "018f47a2-6b2d-7c11-8a44-001122334457"
        );
        assert_eq!(
            handoff.metadata_bytes(),
            recording_metadata_with_extensions(&serde_json::json!({}))
        );
    }

    #[test]
    fn finalized_handoff_parser_preserves_existing_hex_digest_compatibility() {
        let mut value: serde_json::Value = serde_json::from_slice(&finalized_handoff()).unwrap();
        value["recording"]["sha256"] = serde_json::json!("A".repeat(64));
        let bytes = serde_json_canonicalizer::to_vec(&value).unwrap();

        let handoff = FinalizedRecordingHandoff::parse(&bytes).unwrap();
        assert_eq!(handoff.recording_sha256(), &[0xaa; 32]);
        assert_eq!(handoff.canonical_bytes(), bytes);
    }

    #[test]
    fn finalized_handoff_parser_refuses_noncanonical_and_untrusted_domains() {
        let bytes = finalized_handoff();
        let mut noncanonical = Vec::with_capacity(bytes.len() + 1);
        noncanonical.push(b' ');
        noncanonical.extend_from_slice(&bytes);
        assert_eq!(
            FinalizedRecordingHandoff::parse(&noncanonical)
                .unwrap_err()
                .kind(),
            FinalizedRecordingHandoffParseErrorKind::NonCanonicalJson
        );

        let mut relative_path: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        relative_path["recording"]["path"] = serde_json::json!("recorded-take.wav");
        let relative_path = serde_json_canonicalizer::to_vec(&relative_path).unwrap();
        assert_eq!(
            FinalizedRecordingHandoff::parse(&relative_path)
                .unwrap_err()
                .kind(),
            FinalizedRecordingHandoffParseErrorKind::InvalidDomain
        );

        let mut invalid_digest: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        invalid_digest["recording"]["sha256"] = serde_json::json!("z".repeat(64));
        let invalid_digest = serde_json_canonicalizer::to_vec(&invalid_digest).unwrap();
        assert_eq!(
            FinalizedRecordingHandoff::parse(&invalid_digest)
                .unwrap_err()
                .kind(),
            FinalizedRecordingHandoffParseErrorKind::InvalidDomain
        );

        let mut unknown_metadata: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        unknown_metadata["metadata"]["unexpected"] = serde_json::json!(true);
        let unknown_metadata = serde_json_canonicalizer::to_vec(&unknown_metadata).unwrap();
        assert!(matches!(
            FinalizedRecordingHandoff::parse(&unknown_metadata)
                .unwrap_err()
                .kind(),
            FinalizedRecordingHandoffParseErrorKind::Metadata(_)
        ));
    }

    #[test]
    fn finalized_handoff_parser_enforces_the_transport_byte_boundary() {
        let oversized = vec![b' '; MAX_FINALIZED_RECORDING_HANDOFF_BYTES + 1];
        assert_eq!(
            FinalizedRecordingHandoff::parse(&oversized)
                .unwrap_err()
                .kind(),
            FinalizedRecordingHandoffParseErrorKind::LimitExceeded
        );
    }

    #[test]
    fn take_label_rewrite_is_canonical_and_preserves_extensions() {
        let source_extensions = serde_json::json!({
            "org.example.fixture": { "retained": true },
            TAKE_LABEL_EXTENSION: { "label": "Original", "future": 7 },
        });
        let source = recording_metadata_with_extensions(&source_extensions);
        let parsed = RecordingMetadataDocument::parse(&source).unwrap();
        assert_eq!(parsed.take_label(), Some("Original"));

        let renamed = parsed.with_take_label("Lead vocal").unwrap();
        let value: serde_json::Value = serde_json::from_slice(&renamed).unwrap();
        assert_eq!(
            value["extensions"]["org.example.fixture"],
            serde_json::json!({ "retained": true })
        );
        assert_eq!(
            value["extensions"][TAKE_LABEL_EXTENSION]["future"],
            serde_json::json!(7)
        );
        assert_eq!(
            value["extensions"][TAKE_LABEL_EXTENSION]["label"],
            serde_json::json!("Lead vocal")
        );
        assert_eq!(
            renamed,
            serde_json_canonicalizer::to_vec(&value).unwrap(),
            "the rewrite must remain RFC 8785 canonical"
        );
        assert_eq!(
            RecordingMetadataDocument::parse(&renamed)
                .unwrap()
                .take_label(),
            Some("Lead vocal")
        );
    }

    #[test]
    fn take_label_extension_rejects_invalid_values() {
        for extension in [
            serde_json::json!(false),
            serde_json::json!({}),
            serde_json::json!({ "label": 1 }),
            serde_json::json!({ "label": " leading" }),
            serde_json::json!({ "label": "x".repeat(121) }),
        ] {
            let extensions = serde_json::json!({
                TAKE_LABEL_EXTENSION: extension,
            });
            let source = recording_metadata_with_extensions(&extensions);
            assert!(RecordingMetadataDocument::parse(&source).is_err());
        }

        let empty_extensions = serde_json::json!({});
        let parsed = RecordingMetadataDocument::parse(&recording_metadata_with_extensions(
            &empty_extensions,
        ))
        .unwrap();
        for label in ["", " ", " label", "label "] {
            assert!(matches!(
                parsed.with_take_label(label),
                Err(RecordingMetadataEditError::InvalidTakeLabel)
            ));
        }
    }

    #[test]
    fn handoff_paths_share_one_utf8_boundary_contract() {
        assert!(matches!(
            handoff_publication_path(Path::new("relative.wav")),
            Err(RecordingHandoffError::NonAbsolutePublicationPath)
        ));

        let unicode = format!("{ABSOLUTE_PREFIX}registrazione-\u{e8}.wav");
        assert_eq!(
            handoff_publication_path(Path::new(&unicode)).unwrap(),
            unicode
        );

        let exact = format!(
            "{ABSOLUTE_PREFIX}{}",
            "a".repeat(MAX_FINALIZED_RECORDING_HANDOFF_PATH_UTF8_BYTES - ABSOLUTE_PREFIX.len())
        );
        assert_eq!(exact.len(), MAX_FINALIZED_RECORDING_HANDOFF_PATH_UTF8_BYTES);
        assert!(handoff_publication_path(Path::new(&exact)).is_ok());

        let oversized = format!("{exact}a");
        assert!(matches!(
            handoff_publication_path(Path::new(&oversized)),
            Err(RecordingHandoffError::PublicationPathTooLong {
                actual_bytes: 4097,
                maximum: 4096
            })
        ));

        let nul = format!("{ABSOLUTE_PREFIX}bad\0.wav");
        assert!(matches!(
            handoff_publication_path(Path::new(&nul)),
            Err(RecordingHandoffError::PublicationPathContainsNul)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn handoff_path_must_be_lossless_unicode() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        use std::path::PathBuf;

        let invalid = PathBuf::from(OsString::from_vec(vec![b'/', 0xff]));
        assert!(matches!(
            handoff_publication_path(&invalid),
            Err(RecordingHandoffError::NonUnicodePublicationPath)
        ));
    }
}
