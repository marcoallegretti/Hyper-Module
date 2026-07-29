#![doc = "Recoverable recording journals and verified WAV publication."]

use crc32fast::Hasher;
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const JOURNAL_MAGIC: &[u8; 8] = b"HYPRREC\0";
const JOURNAL_VERSION: u16 = 1;
const JOURNAL_HEADER_SIZE: usize = 36;
const JOURNAL_HEADER_SIZE_U16: u16 = 36;
const CHUNK_MAGIC: &[u8; 4] = b"CHNK";
const CHUNK_HEADER_SIZE: usize = 32;
const CHUNK_HEADER_SIZE_U16: u16 = 32;
const CHANNELS: u16 = 2;
const BYTES_PER_FRAME: u64 = 8;
const BYTES_PER_FRAME_U32: u32 = 8;
const BYTES_PER_FRAME_USIZE: usize = 8;
const MIN_SAMPLE_RATE: u32 = 8_000;
const MAX_SAMPLE_RATE: u32 = 384_000;
const MAX_WAV_DATA_BYTES: u64 = u32::MAX as u64 - 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JournalConfig {
    pub sample_rate: u32,
    pub max_chunk_frames: u32,
    pub max_total_frames: u64,
}

impl JournalConfig {
    /// Checks container, allocation, and supported sample-rate bounds.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError::InvalidConfig`] when a bound is zero,
    /// unsupported, internally inconsistent, or cannot fit a WAV container.
    pub fn validate(self) -> Result<Self, JournalError> {
        if !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&self.sample_rate) {
            return Err(JournalError::InvalidConfig(
                "sample rate must be between 8000 and 384000 Hz",
            ));
        }
        if self.max_chunk_frames == 0 {
            return Err(JournalError::InvalidConfig(
                "maximum chunk frames must be nonzero",
            ));
        }
        if self.max_total_frames == 0 {
            return Err(JournalError::InvalidConfig(
                "maximum total frames must be nonzero",
            ));
        }
        if u64::from(self.max_chunk_frames) > self.max_total_frames {
            return Err(JournalError::InvalidConfig(
                "maximum chunk frames exceeds maximum total frames",
            ));
        }
        if self
            .max_total_frames
            .checked_mul(BYTES_PER_FRAME)
            .is_none_or(|bytes| bytes > MAX_WAV_DATA_BYTES)
        {
            return Err(JournalError::InvalidConfig(
                "maximum total frames exceeds the WAV container limit",
            ));
        }
        Ok(self)
    }
}

#[derive(Debug)]
pub enum JournalError {
    InvalidConfig(&'static str),
    EmptyChunk,
    SampleCountNotStereoAligned { samples: usize },
    NonFiniteSample,
    ChunkTooLarge { frames: u64, maximum: u32 },
    RecordingTooLarge { frames: u64, maximum: u64 },
    JournalExists(PathBuf),
    JournalInUse(PathBuf),
    InvalidJournal(&'static str),
    CorruptJournal { offset: u64, reason: &'static str },
    WriterPoisoned,
    StagingConflict(PathBuf),
    PublishedRecordingConflict(PathBuf),
    Io(io::Error),
    Wav(hound::Error),
}

impl fmt::Display for JournalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(reason) => write!(formatter, "invalid journal config: {reason}"),
            Self::EmptyChunk => formatter.write_str("recording chunk must not be empty"),
            Self::SampleCountNotStereoAligned { samples } => write!(
                formatter,
                "recording chunk has {samples} samples; expected interleaved stereo"
            ),
            Self::NonFiniteSample => {
                formatter.write_str("recording chunk contains a non-finite sample")
            }
            Self::ChunkTooLarge { frames, maximum } => write!(
                formatter,
                "recording chunk has {frames} frames; maximum is {maximum}"
            ),
            Self::RecordingTooLarge { frames, maximum } => write!(
                formatter,
                "recording would have {frames} frames; maximum is {maximum}"
            ),
            Self::JournalExists(path) => {
                write!(
                    formatter,
                    "recording journal already exists: {}",
                    path.display()
                )
            }
            Self::JournalInUse(path) => {
                write!(
                    formatter,
                    "recording journal is already in use: {}",
                    path.display()
                )
            }
            Self::InvalidJournal(reason) => {
                write!(formatter, "invalid recording journal: {reason}")
            }
            Self::CorruptJournal { offset, reason } => {
                write!(
                    formatter,
                    "recording journal is corrupt at byte {offset}: {reason}"
                )
            }
            Self::WriterPoisoned => {
                formatter.write_str("recording journal writer is poisoned by an earlier I/O error")
            }
            Self::StagingConflict(path) => write!(
                formatter,
                "recording staging file does not match the journal: {}",
                path.display()
            ),
            Self::PublishedRecordingConflict(path) => write!(
                formatter,
                "published recording path already contains different media: {}",
                path.display()
            ),
            Self::Io(source) => write!(formatter, "recording journal I/O failed: {source}"),
            Self::Wav(source) => write!(formatter, "recording WAV processing failed: {source}"),
        }
    }
}

impl Error for JournalError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            Self::Wav(source) => Some(source),
            _ => None,
        }
    }
}

impl From<io::Error> for JournalError {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}

impl From<hound::Error> for JournalError {
    fn from(source: hound::Error) -> Self {
        Self::Wav(source)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecoveryReport {
    pub config: JournalConfig,
    pub chunks: u64,
    pub frames: u64,
    pub discarded_bytes: u64,
    pub content_crc32: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicationOutcome {
    Published,
    AlreadyPublished,
}

pub struct JournalWriter {
    path: PathBuf,
    file: BufWriter<File>,
    config: JournalConfig,
    next_sequence: u64,
    committed_frames: u64,
    encoded: Vec<u8>,
    poisoned: bool,
}

impl JournalWriter {
    /// Creates and durably initializes a new journal without replacing a file.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid configuration, allocation failure, an
    /// existing target, or any filesystem failure.
    pub fn create(path: impl AsRef<Path>, config: JournalConfig) -> Result<Self, JournalError> {
        let path = path.as_ref();
        let config = config.validate()?;
        let encoded = allocate_payload_buffer(config)?;
        let file = match OpenOptions::new()
            .write(true)
            .read(true)
            .create_new(true)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(JournalError::JournalExists(path.to_owned()));
            }
            Err(error) => return Err(error.into()),
        };
        if let Err(error) = lock_file(&file, path) {
            drop(file);
            let _ = fs::remove_file(path);
            return Err(error);
        }

        let header = encode_journal_header(config);
        let mut file = BufWriter::new(file);
        if let Err(error) = write_and_sync(&mut file, &header) {
            drop(file);
            let _ = fs::remove_file(path);
            return Err(error.into());
        }

        Ok(Self {
            path: path.to_owned(),
            file,
            config,
            next_sequence: 0,
            committed_frames: 0,
            encoded,
            poisoned: false,
        })
    }

    /// Repairs a torn tail and opens an existing journal for further appends.
    ///
    /// # Errors
    ///
    /// Returns an error when the header or a committed interior chunk is
    /// corrupt, configuration is invalid, allocation fails, or I/O fails.
    pub fn resume(path: impl AsRef<Path>) -> Result<(Self, RecoveryReport), JournalError> {
        let path = path.as_ref();
        let mut file = open_locked(path)?;
        let report = recover_file(&mut file)?;
        file.seek(SeekFrom::End(0))?;
        let encoded = allocate_payload_buffer(report.config)?;
        Ok((
            Self {
                path: path.to_owned(),
                file: BufWriter::new(file),
                config: report.config,
                next_sequence: report.chunks,
                committed_frames: report.frames,
                encoded,
                poisoned: false,
            },
            report,
        ))
    }

    /// Appends one finite, interleaved stereo chunk and syncs it before commit.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed samples, exceeded bounds, or I/O
    /// failure. An I/O failure poisons the writer and prevents later appends.
    pub fn append(&mut self, interleaved_stereo: &[f32]) -> Result<(), JournalError> {
        if self.poisoned {
            return Err(JournalError::WriterPoisoned);
        }
        if interleaved_stereo.is_empty() {
            return Err(JournalError::EmptyChunk);
        }
        if !interleaved_stereo.len().is_multiple_of(CHANNELS as usize) {
            return Err(JournalError::SampleCountNotStereoAligned {
                samples: interleaved_stereo.len(),
            });
        }
        if interleaved_stereo.iter().any(|sample| !sample.is_finite()) {
            return Err(JournalError::NonFiniteSample);
        }
        let frames = (interleaved_stereo.len() / CHANNELS as usize) as u64;
        if frames > u64::from(self.config.max_chunk_frames) {
            return Err(JournalError::ChunkTooLarge {
                frames,
                maximum: self.config.max_chunk_frames,
            });
        }
        let total =
            self.committed_frames
                .checked_add(frames)
                .ok_or(JournalError::RecordingTooLarge {
                    frames: u64::MAX,
                    maximum: self.config.max_total_frames,
                })?;
        if total > self.config.max_total_frames {
            return Err(JournalError::RecordingTooLarge {
                frames: total,
                maximum: self.config.max_total_frames,
            });
        }

        self.encoded.clear();
        for sample in interleaved_stereo {
            self.encoded.extend_from_slice(&sample.to_le_bytes());
        }
        let payload_crc = crc32(&self.encoded);
        let header = encode_chunk_header(
            self.next_sequence,
            u32::try_from(frames).map_err(|_| JournalError::ChunkTooLarge {
                frames,
                maximum: self.config.max_chunk_frames,
            })?,
            u32::try_from(self.encoded.len()).map_err(|_| JournalError::ChunkTooLarge {
                frames,
                maximum: self.config.max_chunk_frames,
            })?,
            payload_crc,
        );
        if let Err(error) = write_pair_and_sync(&mut self.file, &header, &self.encoded) {
            self.poisoned = true;
            return Err(error.into());
        }
        self.next_sequence += 1;
        self.committed_frames = total;
        Ok(())
    }

    #[must_use]
    pub fn committed_frames(&self) -> u64 {
        self.committed_frames
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Verifies committed chunks and durably removes an incomplete trailing chunk.
///
/// # Errors
///
/// Returns an error for an invalid header, interior corruption, exceeded
/// bounds, allocation failure, or filesystem failure.
pub fn recover(path: impl AsRef<Path>) -> Result<RecoveryReport, JournalError> {
    let path = path.as_ref();
    let mut file = open_locked(path)?;
    recover_file(&mut file)
}

fn recover_file(file: &mut File) -> Result<RecoveryReport, JournalError> {
    file.seek(SeekFrom::Start(0))?;
    let length = file.metadata()?.len();
    if length < JOURNAL_HEADER_SIZE as u64 {
        return Err(JournalError::InvalidJournal("header is incomplete"));
    }

    let mut journal_header = [0_u8; JOURNAL_HEADER_SIZE];
    file.read_exact(&mut journal_header)?;
    let config = decode_journal_header(&journal_header)?;
    let mut verified_offset = JOURNAL_HEADER_SIZE as u64;
    let mut sequence = 0_u64;
    let mut frames = 0_u64;
    let mut content_hasher = Hasher::new();
    let mut chunk_header = [0_u8; CHUNK_HEADER_SIZE];
    let mut payload = Vec::new();

    while verified_offset < length {
        let remaining = length - verified_offset;
        if remaining < CHUNK_HEADER_SIZE as u64 {
            return truncate_tail(
                file,
                config,
                sequence,
                frames,
                verified_offset,
                remaining,
                content_hasher,
            );
        }
        file.read_exact(&mut chunk_header)?;
        let decoded = decode_chunk_header(&chunk_header, verified_offset)?;
        if decoded.sequence != sequence {
            return Err(JournalError::CorruptJournal {
                offset: verified_offset,
                reason: "chunk sequence is discontinuous",
            });
        }
        let (chunk_end, next_frames) = validate_chunk(decoded, config, verified_offset, frames)?;
        if chunk_end > length {
            return truncate_tail(
                file,
                config,
                sequence,
                frames,
                verified_offset,
                length - verified_offset,
                content_hasher,
            );
        }
        resize_zeroed(&mut payload, decoded.payload_bytes as usize)?;
        file.read_exact(&mut payload)?;
        if crc32(&payload) != decoded.payload_crc32 {
            if chunk_end == length {
                return truncate_tail(
                    file,
                    config,
                    sequence,
                    frames,
                    verified_offset,
                    length - verified_offset,
                    content_hasher,
                );
            }
            return Err(JournalError::CorruptJournal {
                offset: verified_offset,
                reason: "interior chunk payload checksum failed",
            });
        }
        content_hasher.update(&payload);
        frames = next_frames;
        sequence += 1;
        verified_offset = chunk_end;
    }

    Ok(RecoveryReport {
        config,
        chunks: sequence,
        frames,
        discarded_bytes: 0,
        content_crc32: content_hasher.finalize(),
    })
}

/// Finalizes a verified float WAV and publishes it without replacing media.
///
/// Calling this function again for the same journal and output is idempotent.
///
/// # Errors
///
/// Returns an error for an empty or corrupt journal, conflicting staging or
/// published media, unsupported filesystem links, allocation failure, or I/O.
pub fn publish_wav(
    journal_path: impl AsRef<Path>,
    final_path: impl AsRef<Path>,
) -> Result<PublicationOutcome, JournalError> {
    let journal_path = journal_path.as_ref();
    let final_path = final_path.as_ref();
    let mut journal_lock = open_locked(journal_path)?;
    let report = recover_file(&mut journal_lock)?;
    if report.frames == 0 {
        return Err(JournalError::InvalidJournal(
            "an empty recording cannot be published",
        ));
    }
    let staging_path = staging_path(final_path)?;

    if final_path.exists() {
        if verify_wav(final_path, report) {
            if staging_path.exists() {
                if !verify_wav(&staging_path, report) {
                    return Err(JournalError::StagingConflict(staging_path));
                }
                fs::remove_file(&staging_path)?;
                sync_parent(final_path)?;
            }
            return Ok(PublicationOutcome::AlreadyPublished);
        }
        return Err(JournalError::PublishedRecordingConflict(
            final_path.to_owned(),
        ));
    }

    if staging_path.exists() {
        if !verify_wav(&staging_path, report) {
            return Err(JournalError::StagingConflict(staging_path));
        }
    } else {
        write_staging_wav(&mut journal_lock, &staging_path, report)?;
        if !verify_wav(&staging_path, report) {
            return Err(JournalError::StagingConflict(staging_path));
        }
    }

    match fs::hard_link(&staging_path, final_path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if verify_wav(final_path, report) {
                fs::remove_file(&staging_path)?;
                sync_published(final_path)?;
                return Ok(PublicationOutcome::AlreadyPublished);
            }
            return Err(JournalError::PublishedRecordingConflict(
                final_path.to_owned(),
            ));
        }
        Err(error) => return Err(error.into()),
    }
    sync_published(final_path)?;
    fs::remove_file(&staging_path)?;
    sync_parent(final_path)?;
    Ok(PublicationOutcome::Published)
}

#[derive(Clone, Copy)]
struct ChunkHeader {
    sequence: u64,
    frames: u32,
    payload_bytes: u32,
    payload_crc32: u32,
}

fn encode_journal_header(config: JournalConfig) -> [u8; JOURNAL_HEADER_SIZE] {
    let mut output = [0_u8; JOURNAL_HEADER_SIZE];
    output[0..8].copy_from_slice(JOURNAL_MAGIC);
    output[8..10].copy_from_slice(&JOURNAL_VERSION.to_le_bytes());
    output[10..12].copy_from_slice(&JOURNAL_HEADER_SIZE_U16.to_le_bytes());
    output[12..14].copy_from_slice(&CHANNELS.to_le_bytes());
    output[16..20].copy_from_slice(&config.sample_rate.to_le_bytes());
    output[20..24].copy_from_slice(&config.max_chunk_frames.to_le_bytes());
    output[24..32].copy_from_slice(&config.max_total_frames.to_le_bytes());
    let checksum = crc32(&output[..32]);
    output[32..36].copy_from_slice(&checksum.to_le_bytes());
    output
}

fn decode_journal_header(
    header: &[u8; JOURNAL_HEADER_SIZE],
) -> Result<JournalConfig, JournalError> {
    if &header[0..8] != JOURNAL_MAGIC {
        return Err(JournalError::InvalidJournal("magic does not match"));
    }
    if read_u16(&header[8..10]) != JOURNAL_VERSION {
        return Err(JournalError::InvalidJournal("version is unsupported"));
    }
    if read_u16(&header[10..12]) as usize != JOURNAL_HEADER_SIZE {
        return Err(JournalError::InvalidJournal("header size does not match"));
    }
    if read_u16(&header[12..14]) != CHANNELS || read_u16(&header[14..16]) != 0 {
        return Err(JournalError::InvalidJournal(
            "channel count or reserved fields are invalid",
        ));
    }
    if read_u32(&header[32..36]) != crc32(&header[..32]) {
        return Err(JournalError::InvalidJournal("header checksum failed"));
    }
    JournalConfig {
        sample_rate: read_u32(&header[16..20]),
        max_chunk_frames: read_u32(&header[20..24]),
        max_total_frames: read_u64(&header[24..32]),
    }
    .validate()
}

fn encode_chunk_header(
    sequence: u64,
    frames: u32,
    payload_bytes: u32,
    payload_crc32: u32,
) -> [u8; CHUNK_HEADER_SIZE] {
    let mut output = [0_u8; CHUNK_HEADER_SIZE];
    output[0..4].copy_from_slice(CHUNK_MAGIC);
    output[4..6].copy_from_slice(&CHUNK_HEADER_SIZE_U16.to_le_bytes());
    output[8..16].copy_from_slice(&sequence.to_le_bytes());
    output[16..20].copy_from_slice(&frames.to_le_bytes());
    output[20..24].copy_from_slice(&payload_bytes.to_le_bytes());
    output[24..28].copy_from_slice(&payload_crc32.to_le_bytes());
    let checksum = crc32(&output[..28]);
    output[28..32].copy_from_slice(&checksum.to_le_bytes());
    output
}

fn decode_chunk_header(
    header: &[u8; CHUNK_HEADER_SIZE],
    offset: u64,
) -> Result<ChunkHeader, JournalError> {
    if &header[0..4] != CHUNK_MAGIC {
        return Err(JournalError::CorruptJournal {
            offset,
            reason: "chunk magic does not match",
        });
    }
    if read_u16(&header[4..6]) as usize != CHUNK_HEADER_SIZE || read_u16(&header[6..8]) != 0 {
        return Err(JournalError::CorruptJournal {
            offset,
            reason: "chunk header size or flags are invalid",
        });
    }
    if read_u32(&header[28..32]) != crc32(&header[..28]) {
        return Err(JournalError::CorruptJournal {
            offset,
            reason: "chunk header checksum failed",
        });
    }
    Ok(ChunkHeader {
        sequence: read_u64(&header[8..16]),
        frames: read_u32(&header[16..20]),
        payload_bytes: read_u32(&header[20..24]),
        payload_crc32: read_u32(&header[24..28]),
    })
}

fn validate_chunk(
    chunk: ChunkHeader,
    config: JournalConfig,
    offset: u64,
    committed_frames: u64,
) -> Result<(u64, u64), JournalError> {
    if chunk.frames == 0 || chunk.frames > config.max_chunk_frames {
        return Err(JournalError::CorruptJournal {
            offset,
            reason: "chunk frame count is outside configured bounds",
        });
    }
    let expected_bytes =
        chunk
            .frames
            .checked_mul(BYTES_PER_FRAME_U32)
            .ok_or(JournalError::CorruptJournal {
                offset,
                reason: "chunk payload size overflowed",
            })?;
    if chunk.payload_bytes != expected_bytes {
        return Err(JournalError::CorruptJournal {
            offset,
            reason: "chunk payload size does not match its frame count",
        });
    }
    let end = offset
        .checked_add(CHUNK_HEADER_SIZE as u64)
        .and_then(|value| value.checked_add(u64::from(chunk.payload_bytes)))
        .ok_or(JournalError::CorruptJournal {
            offset,
            reason: "chunk extent overflowed",
        })?;
    let total = committed_frames
        .checked_add(u64::from(chunk.frames))
        .ok_or(JournalError::CorruptJournal {
            offset,
            reason: "total frame count overflowed",
        })?;
    if total > config.max_total_frames {
        return Err(JournalError::CorruptJournal {
            offset,
            reason: "total frame count exceeds configured bounds",
        });
    }
    Ok((end, total))
}

fn truncate_tail(
    file: &File,
    config: JournalConfig,
    chunks: u64,
    frames: u64,
    verified_offset: u64,
    discarded_bytes: u64,
    content_hasher: Hasher,
) -> Result<RecoveryReport, JournalError> {
    file.set_len(verified_offset)?;
    file.sync_all()?;
    Ok(RecoveryReport {
        config,
        chunks,
        frames,
        discarded_bytes,
        content_crc32: content_hasher.finalize(),
    })
}

fn write_staging_wav(
    journal: &mut File,
    staging_path: &Path,
    report: RecoveryReport,
) -> Result<(), JournalError> {
    let staging = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(staging_path)?;
    let result = (|| {
        let spec = hound::WavSpec {
            channels: CHANNELS,
            sample_rate: report.config.sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::new(BufWriter::new(staging), spec)?;
        visit_payloads(journal, report, |payload| {
            for bytes in payload.chunks_exact(4) {
                writer.write_sample(f32::from_le_bytes(
                    bytes.try_into().expect("sample chunk has four bytes"),
                ))?;
            }
            Ok(())
        })?;
        writer.finalize()?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(staging_path)?;
        file.sync_all()?;
        Ok::<(), JournalError>(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(staging_path);
    }
    result
}

fn visit_payloads(
    journal: &mut File,
    report: RecoveryReport,
    mut visitor: impl FnMut(&[u8]) -> Result<(), JournalError>,
) -> Result<(), JournalError> {
    let mut file = BufReader::new(journal);
    file.seek(SeekFrom::Start(JOURNAL_HEADER_SIZE as u64))?;
    let maximum_payload = usize::try_from(report.config.max_chunk_frames)
        .ok()
        .and_then(|frames| frames.checked_mul(BYTES_PER_FRAME_USIZE))
        .ok_or(JournalError::InvalidJournal(
            "maximum chunk payload exceeds addressable memory",
        ))?;
    let mut sequence = 0_u64;
    let mut frames = 0_u64;
    let mut hasher = Hasher::new();
    let mut payload = Vec::new();
    loop {
        let mut header = [0_u8; CHUNK_HEADER_SIZE];
        match file.read_exact(&mut header) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error.into()),
        }
        let decoded =
            decode_chunk_header(&header, file.stream_position()? - CHUNK_HEADER_SIZE as u64)?;
        if decoded.sequence != sequence {
            return Err(JournalError::InvalidJournal(
                "journal changed while it was being finalized",
            ));
        }
        let (_, next_frames) = validate_chunk(
            decoded,
            report.config,
            file.stream_position()? - CHUNK_HEADER_SIZE as u64,
            frames,
        )?;
        let payload_bytes = decoded.payload_bytes as usize;
        if payload_bytes > maximum_payload {
            return Err(JournalError::InvalidJournal(
                "chunk payload exceeds configured bounds",
            ));
        }
        resize_zeroed(&mut payload, payload_bytes)?;
        file.read_exact(&mut payload)?;
        if crc32(&payload) != decoded.payload_crc32 {
            return Err(JournalError::InvalidJournal(
                "journal changed while it was being finalized",
            ));
        }
        frames = next_frames;
        sequence += 1;
        hasher.update(&payload);
        visitor(&payload)?;
    }
    if sequence == report.chunks
        && frames == report.frames
        && hasher.finalize() == report.content_crc32
    {
        Ok(())
    } else {
        Err(JournalError::InvalidJournal(
            "journal changed while it was being finalized",
        ))
    }
}

fn verify_wav(path: &Path, report: RecoveryReport) -> bool {
    let Ok(mut reader) = hound::WavReader::open(path) else {
        return false;
    };
    let spec = reader.spec();
    if spec.channels != CHANNELS
        || spec.sample_rate != report.config.sample_rate
        || spec.bits_per_sample != 32
        || spec.sample_format != hound::SampleFormat::Float
        || u64::from(reader.duration()) != report.frames
    {
        return false;
    }
    let mut hasher = Hasher::new();
    let mut samples = 0_u64;
    for sample in reader.samples::<f32>() {
        let sample = match sample {
            Ok(sample) if sample.is_finite() => sample,
            _ => return false,
        };
        hasher.update(&sample.to_le_bytes());
        samples += 1;
    }
    samples == report.frames * u64::from(CHANNELS) && hasher.finalize() == report.content_crc32
}

fn staging_path(final_path: &Path) -> Result<PathBuf, JournalError> {
    let name = final_path
        .file_name()
        .ok_or(JournalError::InvalidConfig(
            "published WAV path must have a file name",
        ))?
        .to_string_lossy();
    Ok(final_path.with_file_name(format!(".{name}.hyper-staging")))
}

fn sync_published(path: &Path) -> Result<(), JournalError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?
        .sync_all()?;
    sync_parent(path)
}

fn sync_parent(path: &Path) -> Result<(), JournalError> {
    let parent = path.parent().ok_or(JournalError::InvalidConfig(
        "published WAV path must have a parent directory",
    ))?;
    fs::metadata(parent)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn allocate_payload_buffer(config: JournalConfig) -> Result<Vec<u8>, JournalError> {
    let capacity = usize::try_from(config.max_chunk_frames)
        .ok()
        .and_then(|frames| frames.checked_mul(BYTES_PER_FRAME_USIZE))
        .ok_or(JournalError::InvalidConfig(
            "maximum chunk frames exceeds addressable memory",
        ))?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(capacity)
        .map_err(|_| JournalError::InvalidConfig("chunk buffer allocation failed"))?;
    Ok(output)
}

fn resize_zeroed(output: &mut Vec<u8>, length: usize) -> Result<(), JournalError> {
    output
        .try_reserve_exact(length.saturating_sub(output.len()))
        .map_err(|_| JournalError::InvalidJournal("payload allocation failed"))?;
    output.resize(length, 0);
    Ok(())
}

fn open_locked(path: &Path) -> Result<File, JournalError> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    lock_file(&file, path)?;
    Ok(file)
}

fn lock_file(file: &File, path: &Path) -> Result<(), JournalError> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => Err(JournalError::JournalInUse(path.to_owned())),
        Err(TryLockError::Error(error)) => Err(error.into()),
    }
}

fn write_and_sync(writer: &mut BufWriter<File>, bytes: &[u8]) -> io::Result<()> {
    writer.write_all(bytes)?;
    writer.flush()?;
    writer.get_ref().sync_all()
}

fn write_pair_and_sync(
    writer: &mut BufWriter<File>,
    first: &[u8],
    second: &[u8],
) -> io::Result<()> {
    writer.write_all(first)?;
    writer.write_all(second)?;
    writer.flush()?;
    writer.get_ref().sync_data()
}

fn crc32(bytes: &[u8]) -> u32 {
    crc32fast::hash(bytes)
}

fn read_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes(bytes.try_into().expect("u16 field has two bytes"))
}

fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().expect("u32 field has four bytes"))
}

fn read_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes.try_into().expect("u64 field has eight bytes"))
}
