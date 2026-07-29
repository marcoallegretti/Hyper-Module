#![doc = "Validated metadata for verified recording publications."]

use hyper_recording_journal::PublishedRecording;
use serde::Serialize;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

const FORMAT: &str = "hyper-recording-metadata";
const FORMAT_VERSION: u8 = 1;
const VERSION_ONE_SAMPLE_RATE: u32 = 48_000;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordingCaptureKind {
    New,
    Overdub,
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
        serde_json::to_vec(&CanonicalMetadata {
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
        })
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

fn validate_id(field: &'static str, value: &str) -> Result<(), RecordingMetadataError> {
    let bytes = value.as_bytes();
    let valid = bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(byte),
        });
    if valid {
        Ok(())
    } else {
        Err(RecordingMetadataError::InvalidId(field))
    }
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
