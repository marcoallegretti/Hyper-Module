#![doc = "Recoverable recording journals and verified WAV publication."]

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions as CapOpenOptions};
use crc32fast::Hasher;
use same_file::Handle;
use sha2::{Digest, Sha256};
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
#[cfg(windows)]
use std::fs::OpenOptions;
use std::fs::{self, File, TryLockError};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;

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

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_link {
    use std::ffi::OsStr;
    use std::io;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use std::ptr::{addr_of_mut, copy_nonoverlapping};

    use cap_std::fs::Dir;
    use same_file::Handle;
    use windows_sys::Wdk::Storage::FileSystem::{
        FILE_LINK_INFORMATION, FileLinkInformation, NtSetInformationFile,
    };
    use windows_sys::Win32::Foundation::RtlNtStatusToDosError;
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    fn invalid_argument(message: &'static str) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidInput, message)
    }

    pub(super) fn hard_link(
        source: &Handle,
        destination_directory: &Dir,
        destination: &OsStr,
    ) -> io::Result<()> {
        let mut destination_name: Vec<u16> = destination.encode_wide().collect();
        let name_bytes = destination_name
            .len()
            .checked_mul(size_of::<u16>())
            .and_then(|length| u32::try_from(length).ok())
            .ok_or_else(|| invalid_argument("destination entry name is too long"))?;
        destination_name.push(0);
        let buffer_bytes = size_of::<FILE_LINK_INFORMATION>()
            .checked_add(name_bytes as usize)
            .ok_or_else(|| invalid_argument("link information size overflow"))?;
        let storage_words = buffer_bytes.div_ceil(size_of::<usize>());
        let mut storage = vec![0_usize; storage_words];
        let information = storage.as_mut_ptr().cast::<FILE_LINK_INFORMATION>();
        let mut io_status = IO_STATUS_BLOCK::default();

        // SAFETY: `storage` is aligned and sized for the fixed information
        // header plus the complete UTF-16 name. Both handles remain owned and
        // valid for the system call, and the destination name is interpreted
        // relative to the retained directory handle.
        let status = unsafe {
            (*information).Anonymous.ReplaceIfExists = false;
            (*information).RootDirectory = destination_directory.as_raw_handle();
            (*information).FileNameLength = name_bytes;
            copy_nonoverlapping(
                destination_name.as_ptr(),
                addr_of_mut!((*information).FileName).cast::<u16>(),
                destination_name.len(),
            );
            NtSetInformationFile(
                source.as_raw_handle(),
                &raw mut io_status,
                information.cast(),
                u32::try_from(buffer_bytes)
                    .map_err(|_| invalid_argument("link information is too large"))?,
                FileLinkInformation,
            )
        };
        if status >= 0 {
            return Ok(());
        }
        // SAFETY: `status` came directly from the NT system service.
        let error = unsafe { RtlNtStatusToDosError(status) };
        Err(io::Error::from_raw_os_error(
            i32::try_from(error).unwrap_or(i32::MAX),
        ))
    }
}

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishedRecording {
    journal_path: PathBuf,
    final_wav_path: PathBuf,
    sample_rate_hz: u32,
    channels: u16,
    frames: u64,
    content_crc32: u32,
    byte_length: u64,
    wav_sha256: [u8; 32],
    outcome: PublicationOutcome,
}

/// A move-only authority for one not-yet-created or existing recording file.
pub struct RecordingPathAuthority {
    path: PathBuf,
    parent: Dir,
    name: OsString,
}

impl fmt::Debug for RecordingPathAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecordingPathAuthority")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl RecordingPathAuthority {
    /// Retains the canonical parent directory and one final path component.
    ///
    /// # Errors
    ///
    /// Returns an error when the path has no ordinary file name or its parent
    /// cannot be canonicalized and retained.
    pub fn retain(path: impl AsRef<Path>) -> Result<Self, JournalError> {
        let path = path.as_ref();
        let name = path
            .file_name()
            .ok_or(JournalError::InvalidConfig(
                "recording path must have a file name",
            ))?
            .to_os_string();
        let mut components = Path::new(&name).components();
        if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
            return Err(JournalError::InvalidConfig(
                "recording file name must be one normal path component",
            ));
        }
        let parent_path = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let display_path = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()?.join(path)
        };
        let canonical_parent = fs::canonicalize(parent_path)?;
        let parent = open_directory_authority(&canonical_parent)?;
        if !parent.dir_metadata()?.is_dir() {
            return Err(JournalError::InvalidConfig(
                "recording path parent must be a directory",
            ));
        }
        Ok(Self {
            path: display_path,
            parent,
            name,
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl PublishedRecording {
    #[must_use]
    pub fn journal_path(&self) -> &Path {
        &self.journal_path
    }

    #[must_use]
    pub fn final_wav_path(&self) -> &Path {
        &self.final_wav_path
    }

    #[must_use]
    pub const fn sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz
    }

    #[must_use]
    pub const fn channels(&self) -> u16 {
        self.channels
    }

    #[must_use]
    pub const fn frames(&self) -> u64 {
        self.frames
    }

    #[must_use]
    pub const fn content_crc32(&self) -> u32 {
        self.content_crc32
    }

    #[must_use]
    pub const fn byte_length(&self) -> u64 {
        self.byte_length
    }

    #[must_use]
    pub const fn wav_sha256(&self) -> &[u8; 32] {
        &self.wav_sha256
    }

    #[must_use]
    pub const fn outcome(&self) -> PublicationOutcome {
        self.outcome
    }
}

pub struct JournalWriter {
    authority: RecordingPathAuthority,
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
        Self::create_retained(RecordingPathAuthority::retain(path)?, config)
    }

    /// Creates a journal beneath a parent directory retained by the caller.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid configuration, allocation failure, an
    /// existing target, or any filesystem failure.
    pub fn create_retained(
        authority: RecordingPathAuthority,
        config: JournalConfig,
    ) -> Result<Self, JournalError> {
        let config = config.validate()?;
        let encoded = allocate_payload_buffer(config)?;
        let mut options = CapOpenOptions::new();
        options.write(true).read(true).create_new(true);
        let file = match authority.parent.open_with(&authority.name, &options) {
            Ok(file) => file.into_std(),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(JournalError::JournalExists(authority.path.clone()));
            }
            Err(error) => return Err(error.into()),
        };
        if let Err(error) = lock_file(&file, &authority.path) {
            drop(file);
            let _ = authority.parent.remove_file(&authority.name);
            return Err(error);
        }

        let header = encode_journal_header(config);
        let mut file = BufWriter::new(file);
        if let Err(error) = write_and_sync(&mut file, &header) {
            drop(file);
            let _ = authority.parent.remove_file(&authority.name);
            return Err(error.into());
        }
        if let Err(error) = sync_directory(&authority.parent) {
            drop(file);
            let _ = authority.parent.remove_file(&authority.name);
            return Err(error);
        }

        Ok(Self {
            authority,
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
        let authority = RecordingPathAuthority::retain(path)?;
        let mut file = open_locked_at(&authority)?;
        let report = recover_file(&mut file)?;
        file.seek(SeekFrom::End(0))?;
        let encoded = allocate_payload_buffer(report.config)?;
        Ok((
            Self {
                authority,
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

    /// Consumes and durably removes this still-owned journal.
    ///
    /// # Errors
    ///
    /// Returns an error when buffered data, removal, or parent-directory
    /// synchronization fails.
    pub fn discard(mut self) -> Result<PathBuf, JournalError> {
        self.file.flush()?;
        let path = self.authority.path.clone();
        drop(self.file);
        self.authority.parent.remove_file(&self.authority.name)?;
        sync_directory(&self.authority.parent)?;
        Ok(path)
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        self.authority.path()
    }

    /// Consumes this writer and publishes WAV from the still-locked journal.
    ///
    /// # Errors
    ///
    /// Returns an error when the writer is poisoned, the journal is empty or
    /// corrupt, publication conflicts, allocation fails, or I/O fails.
    pub fn publish_wav(
        mut self,
        final_path: impl AsRef<Path>,
    ) -> Result<PublishedRecording, JournalError> {
        if self.poisoned {
            return Err(JournalError::WriterPoisoned);
        }
        self.file.flush()?;
        let journal_path = self.authority.path.clone();
        let journal = self.file.get_mut();
        let report = recover_file(journal)?;
        publish_locked_wav(&journal_path, journal, final_path.as_ref(), report)
    }

    /// Publishes WAV while retaining the destination parent authority.
    ///
    /// # Errors
    ///
    /// Returns the same validation, conflict, allocation, and I/O errors as
    /// [`Self::publish_wav`].
    pub fn publish_wav_retained(
        mut self,
        final_authority: &RecordingPathAuthority,
    ) -> Result<PublishedRecording, JournalError> {
        if self.poisoned {
            return Err(JournalError::WriterPoisoned);
        }
        self.file.flush()?;
        let journal_path = self.authority.path.clone();
        let journal = self.file.get_mut();
        let report = recover_file(journal)?;
        publish_locked_wav_retained(&journal_path, journal, final_authority, report)
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
            return Err(JournalError::CorruptJournal {
                offset: verified_offset,
                reason: if chunk_end == length {
                    "final chunk payload checksum failed"
                } else {
                    "interior chunk payload checksum failed"
                },
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
) -> Result<PublishedRecording, JournalError> {
    let journal_path = journal_path.as_ref();
    let final_path = final_path.as_ref();
    let mut journal_lock = open_locked(journal_path)?;
    let report = recover_file(&mut journal_lock)?;
    publish_locked_wav(journal_path, &mut journal_lock, final_path, report)
}

fn publish_locked_wav(
    journal_path: &Path,
    journal_lock: &mut File,
    final_path: &Path,
    report: RecoveryReport,
) -> Result<PublishedRecording, JournalError> {
    let final_authority = RecordingPathAuthority::retain(final_path)?;
    publish_locked_wav_retained(journal_path, journal_lock, &final_authority, report)
}

#[cfg(test)]
fn publish_locked_wav_observed(
    journal_path: &Path,
    journal_lock: &mut File,
    final_path: &Path,
    report: RecoveryReport,
    before_link: impl FnOnce(&Path),
    after_link: impl FnOnce(&Path),
) -> Result<PublishedRecording, JournalError> {
    let final_authority = RecordingPathAuthority::retain(final_path)?;
    publish_locked_wav_retained_observed(
        journal_path,
        journal_lock,
        &final_authority,
        report,
        before_link,
        after_link,
    )
}

fn publish_locked_wav_retained(
    journal_path: &Path,
    journal_lock: &mut File,
    final_authority: &RecordingPathAuthority,
    report: RecoveryReport,
) -> Result<PublishedRecording, JournalError> {
    publish_locked_wav_retained_observed(
        journal_path,
        journal_lock,
        final_authority,
        report,
        |_| {},
        |_| {},
    )
}

fn publish_locked_wav_retained_observed(
    journal_path: &Path,
    journal_lock: &mut File,
    final_authority: &RecordingPathAuthority,
    report: RecoveryReport,
    before_link: impl FnOnce(&Path),
    after_link: impl FnOnce(&Path),
) -> Result<PublishedRecording, JournalError> {
    if report.frames == 0 {
        return Err(JournalError::InvalidJournal(
            "an empty recording cannot be published",
        ));
    }
    let final_path = final_authority.path();
    let staging_name = staging_name(&final_authority.name);
    let staging_path = final_path.with_file_name(&staging_name);

    if let Some(verified) =
        verify_existing_publication(final_authority, &staging_name, &staging_path, report)?
    {
        return Ok(published_recording(
            journal_path,
            final_path,
            report,
            verified,
            PublicationOutcome::AlreadyPublished,
        ));
    }

    let staging_authority = if entry_exists(&final_authority.parent, &staging_name)? {
        verify_wav_authority_at(&final_authority.parent, &staging_name, report)
            .ok_or_else(|| JournalError::StagingConflict(staging_path.clone()))?
    } else {
        write_staging_wav_at(journal_lock, &final_authority.parent, &staging_name, report)?;
        verify_wav_authority_at(&final_authority.parent, &staging_name, report)
            .ok_or_else(|| JournalError::StagingConflict(staging_path.clone()))?
    };

    before_link(&staging_path);
    match hard_link_verified(
        &staging_authority.handle,
        &final_authority.parent,
        &final_authority.name,
    ) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if let Some(verified) =
                verify_wav_at(&final_authority.parent, &final_authority.name, report)
            {
                remove_path_if_owned_at(&staging_authority.handle, final_authority, &staging_name)?;
                sync_published_at(final_authority)?;
                return Ok(published_recording(
                    journal_path,
                    final_path,
                    report,
                    verified,
                    PublicationOutcome::AlreadyPublished,
                ));
            }
            return Err(JournalError::PublishedRecordingConflict(
                final_path.to_owned(),
            ));
        }
        Err(error) => return Err(error.into()),
    }
    after_link(final_path);
    let published = open_verification_handle_at(&final_authority.parent, &final_authority.name)?;
    if published != staging_authority.handle {
        remove_path_if_owned_at(
            &staging_authority.handle,
            final_authority,
            &final_authority.name,
        )?;
        return Err(JournalError::PublishedRecordingConflict(
            final_path.to_owned(),
        ));
    }
    sync_published_at(final_authority)?;
    let verified = verify_wav_at(&final_authority.parent, &final_authority.name, report)
        .ok_or_else(|| JournalError::PublishedRecordingConflict(final_path.to_owned()))?;
    remove_path_if_owned_at(&staging_authority.handle, final_authority, &staging_name)?;
    sync_directory(&final_authority.parent)?;
    Ok(published_recording(
        journal_path,
        final_path,
        report,
        verified,
        PublicationOutcome::Published,
    ))
}

fn verify_existing_publication(
    final_authority: &RecordingPathAuthority,
    staging_name: &OsStr,
    staging_path: &Path,
    report: RecoveryReport,
) -> Result<Option<VerifiedWav>, JournalError> {
    if !entry_exists(&final_authority.parent, &final_authority.name)? {
        return Ok(None);
    }
    let verified = verify_wav_at(&final_authority.parent, &final_authority.name, report)
        .ok_or_else(|| {
            JournalError::PublishedRecordingConflict(final_authority.path().to_owned())
        })?;
    if entry_exists(&final_authority.parent, staging_name)? {
        if verify_wav_at(&final_authority.parent, staging_name, report).is_none() {
            return Err(JournalError::StagingConflict(staging_path.to_owned()));
        }
        final_authority.parent.remove_file(staging_name)?;
        sync_directory(&final_authority.parent)?;
    }
    Ok(Some(verified))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct VerifiedWav {
    byte_length: u64,
    sha256: [u8; 32],
}

struct VerifiedWavAuthority {
    verified: VerifiedWav,
    handle: Handle,
}

fn published_recording(
    journal_path: &Path,
    final_wav_path: &Path,
    report: RecoveryReport,
    verified: VerifiedWav,
    outcome: PublicationOutcome,
) -> PublishedRecording {
    PublishedRecording {
        journal_path: journal_path.to_owned(),
        final_wav_path: final_wav_path.to_owned(),
        sample_rate_hz: report.config.sample_rate,
        channels: CHANNELS,
        frames: report.frames,
        content_crc32: report.content_crc32,
        byte_length: verified.byte_length,
        wav_sha256: verified.sha256,
        outcome,
    }
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

fn write_staging_wav_at(
    journal: &mut File,
    parent: &Dir,
    staging_name: &OsStr,
    report: RecoveryReport,
) -> Result<(), JournalError> {
    let mut options = CapOpenOptions::new();
    options.write(true).read(true).create_new(true);
    let staging = parent.open_with(staging_name, &options)?.into_std();
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
        let mut options = CapOpenOptions::new();
        options.read(true).write(true).follow(FollowSymlinks::No);
        let file = parent.open_with(staging_name, &options)?.into_std();
        file.sync_all()?;
        sync_directory(parent)?;
        Ok::<(), JournalError>(())
    })();
    if result.is_err() {
        let _ = parent.remove_file(staging_name);
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

#[cfg(test)]
fn verify_wav_observed(
    path: &Path,
    report: RecoveryReport,
    after_samples: impl FnOnce(),
) -> Option<VerifiedWav> {
    let authority = RecordingPathAuthority::retain(path).ok()?;
    verify_wav_authority_at_observed(&authority.parent, &authority.name, report, after_samples)
        .map(|verified| verified.verified)
}

fn verify_wav_at(parent: &Dir, name: &OsStr, report: RecoveryReport) -> Option<VerifiedWav> {
    verify_wav_authority_at(parent, name, report).map(|authority| authority.verified)
}

fn verify_wav_authority_at(
    parent: &Dir,
    name: &OsStr,
    report: RecoveryReport,
) -> Option<VerifiedWavAuthority> {
    verify_wav_authority_at_observed(parent, name, report, || {})
}

fn verify_wav_authority_at_observed(
    parent: &Dir,
    name: &OsStr,
    report: RecoveryReport,
    after_samples: impl FnOnce(),
) -> Option<VerifiedWavAuthority> {
    let path_before = parent.symlink_metadata(name).ok()?;
    if path_before.is_symlink() || !path_before.is_file() {
        return None;
    }
    let mut handle = open_verification_handle_at(parent, name).ok()?;
    let file_before = handle.as_file().metadata().ok()?;
    if !file_before.is_file() || file_before.len() != path_before.len() {
        return None;
    }
    let before = hash_open_file(handle.as_file_mut())?;
    handle.as_file_mut().seek(SeekFrom::Start(0)).ok()?;
    if !verify_wav_samples(handle.as_file_mut(), report) {
        return None;
    }
    after_samples();
    handle.as_file_mut().seek(SeekFrom::Start(0)).ok()?;
    let after = hash_open_file(handle.as_file_mut())?;
    if before != after {
        return None;
    }
    let file_after = handle.as_file().metadata().ok()?;
    let path_after = parent.symlink_metadata(name).ok()?;
    if path_after.is_symlink()
        || !path_after.is_file()
        || file_after.len() != file_before.len()
        || metadata_stamp(&file_after) != metadata_stamp(&file_before)
        || path_after.len() != file_after.len()
        || open_verification_handle_at(parent, name).ok()? != handle
    {
        return None;
    }
    Some(VerifiedWavAuthority {
        verified: after,
        handle,
    })
}

fn open_verification_handle_at(parent: &Dir, name: &OsStr) -> io::Result<Handle> {
    let mut options = CapOpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(windows)]
    {
        use cap_std::fs::OpenOptionsExt;

        const DELETE: u32 = 0x0001_0000;
        const GENERIC_READ: u32 = 0x8000_0000;
        const FILE_SHARE_READ: u32 = 1;
        const FILE_SHARE_WRITE: u32 = 2;
        const FILE_SHARE_DELETE: u32 = 4;
        options
            .access_mode(GENERIC_READ | DELETE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
    }
    Handle::from_file(parent.open_with(name, &options)?.into_std())
}

#[cfg(target_os = "linux")]
fn hard_link_verified(source: &Handle, destination: &Dir, name: &OsStr) -> io::Result<()> {
    use rustix::fs::{AtFlags, CWD, linkat};
    use std::os::fd::AsRawFd;

    let source_path = format!("/proc/self/fd/{}", source.as_raw_fd());
    linkat(
        CWD,
        source_path,
        destination,
        Path::new(name),
        AtFlags::SYMLINK_FOLLOW,
    )?;
    Ok(())
}

#[cfg(windows)]
fn hard_link_verified(source: &Handle, destination: &Dir, name: &OsStr) -> io::Result<()> {
    windows_link::hard_link(source, destination, name)
}

fn remove_path_if_owned_at(
    authority: &Handle,
    target: &RecordingPathAuthority,
    name: &OsStr,
) -> Result<bool, JournalError> {
    let mut quarantine = OsString::from(".");
    quarantine.push(name);
    quarantine.push(format!(".hyper-cleanup-{}", Uuid::new_v4()));
    match target.parent.rename(name, &target.parent, &quarantine) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    let moved = open_verification_handle_at(&target.parent, &quarantine)?;
    if &moved != authority {
        drop(moved);
        if !entry_exists(&target.parent, name)? {
            target.parent.rename(&quarantine, &target.parent, name)?;
        }
        return Ok(false);
    }
    drop(moved);
    target.parent.remove_file(&quarantine)?;
    sync_directory(&target.parent)?;
    Ok(true)
}

fn verify_wav_samples(file: &mut File, report: RecoveryReport) -> bool {
    let Ok(mut reader) = hound::WavReader::new(file) else {
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
        let Some(next) = samples.checked_add(1) else {
            return false;
        };
        samples = next;
    }
    report
        .frames
        .checked_mul(u64::from(CHANNELS))
        .is_some_and(|expected| samples == expected)
        && hasher.finalize() == report.content_crc32
}

fn hash_open_file(file: &mut File) -> Option<VerifiedWav> {
    let mut hasher = Sha256::new();
    let mut byte_length = 0_u64;
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        byte_length = byte_length.checked_add(read as u64)?;
        hasher.update(&buffer[..read]);
    }
    Some(VerifiedWav {
        byte_length,
        sha256: hasher.finalize().into(),
    })
}

#[cfg(unix)]
type MetadataStamp = (i64, i64, i64, i64);

#[cfg(unix)]
fn metadata_stamp(metadata: &fs::Metadata) -> MetadataStamp {
    use std::os::unix::fs::MetadataExt;
    (
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    )
}

#[cfg(windows)]
type MetadataStamp = (u64, u64, u32);

#[cfg(windows)]
fn metadata_stamp(metadata: &fs::Metadata) -> MetadataStamp {
    use std::os::windows::fs::MetadataExt;
    (
        metadata.last_write_time(),
        metadata.creation_time(),
        metadata.file_attributes(),
    )
}

fn entry_exists(parent: &Dir, name: &OsStr) -> Result<bool, JournalError> {
    match parent.symlink_metadata(name) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn staging_name(final_name: &OsStr) -> OsString {
    let mut staging = OsString::from(".");
    staging.push(final_name);
    staging.push(".hyper-staging");
    staging
}

fn sync_published_at(authority: &RecordingPathAuthority) -> Result<(), JournalError> {
    let mut options = CapOpenOptions::new();
    options.read(true).write(true).follow(FollowSymlinks::No);
    authority
        .parent
        .open_with(&authority.name, &options)?
        .into_std()
        .sync_all()?;
    sync_directory(&authority.parent)?;
    Ok(())
}

fn sync_directory(directory: &Dir) -> Result<(), JournalError> {
    directory.try_clone()?.into_std_file().sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn open_directory_authority(path: &Path) -> io::Result<Dir> {
    Ok(Dir::from_std_file(File::open(path)?))
}

#[cfg(windows)]
fn open_directory_authority(path: &Path) -> io::Result<Dir> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_SHARE_READ: u32 = 1;
    const FILE_SHARE_WRITE: u32 = 2;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    Ok(Dir::from_std_file(file))
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
    let authority = RecordingPathAuthority::retain(path)?;
    open_locked_at(&authority)
}

fn open_locked_at(authority: &RecordingPathAuthority) -> Result<File, JournalError> {
    let mut options = CapOpenOptions::new();
    options.read(true).write(true).follow(FollowSymlinks::No);
    let file = authority
        .parent
        .open_with(&authority.name, &options)?
        .into_std();
    lock_file(&file, authority.path())?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

    fn test_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "hyper-{label}-{}-{}",
            std::process::id(),
            NEXT_TEST.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn wav_verification_rejects_a_byte_identical_path_replacement() {
        let root = test_root("wav-identity");
        fs::create_dir(&root).unwrap();
        let path = root.join("take.wav");
        let replacement = root.join("replacement.wav");
        let displaced = root.join("displaced.wav");
        let samples = [0.25_f32, -0.25_f32];
        let spec = hound::WavSpec {
            channels: CHANNELS,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for sample in samples {
            writer.write_sample(sample).unwrap();
        }
        writer.finalize().unwrap();
        fs::copy(&path, &replacement).unwrap();
        let mut content = Vec::new();
        for sample in samples {
            content.extend_from_slice(&sample.to_le_bytes());
        }
        let report = RecoveryReport {
            config: JournalConfig {
                sample_rate: 48_000,
                max_chunk_frames: 1,
                max_total_frames: 1,
            },
            chunks: 1,
            frames: 1,
            discarded_bytes: 0,
            content_crc32: crc32(&content),
        };

        let verified = verify_wav_observed(&path, report, || {
            fs::rename(&path, &displaced).unwrap();
            fs::rename(&replacement, &path).unwrap();
        });

        assert!(verified.is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn publication_links_the_verified_file_when_the_staging_path_is_replaced() {
        let root = test_root("staging-publication");
        fs::create_dir(&root).unwrap();
        let journal = root.join("take.journal");
        let final_path = root.join("take.wav");
        let staging = root.join(".take.wav.hyper-staging");
        let displaced = root.join("displaced.wav");
        let replacement = root.join("replacement.wav");
        let samples = [0.25_f32, -0.25_f32];
        let config = JournalConfig {
            sample_rate: 48_000,
            max_chunk_frames: 1,
            max_total_frames: 1,
        };
        let mut writer = JournalWriter::create(&journal, config).unwrap();
        writer.append(&samples).unwrap();
        drop(writer);
        fs::write(&replacement, b"foreign staging data").unwrap();
        let mut journal_lock = open_locked(&journal).unwrap();
        let report = recover_file(&mut journal_lock).unwrap();

        let published = publish_locked_wav_observed(
            &journal,
            &mut journal_lock,
            &final_path,
            report,
            |_| {
                fs::rename(&staging, &displaced).unwrap();
                fs::rename(&replacement, &staging).unwrap();
            },
            |_| {},
        )
        .unwrap();

        assert_eq!(published.outcome(), PublicationOutcome::Published);
        let decoded = hound::WavReader::open(&final_path)
            .unwrap()
            .samples::<f32>()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            decoded
                .iter()
                .map(|sample| sample.to_bits())
                .collect::<Vec<_>>(),
            samples
                .iter()
                .map(|sample| sample.to_bits())
                .collect::<Vec<_>>()
        );
        assert_eq!(fs::read(&staging).unwrap(), b"foreign staging data");
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn publication_never_removes_a_foreign_final_path_replacement() {
        let root = test_root("final-publication");
        fs::create_dir(&root).unwrap();
        let journal = root.join("take.journal");
        let final_path = root.join("take.wav");
        let linked_output = root.join("linked-output.wav");
        let replacement = root.join("replacement.wav");
        let config = JournalConfig {
            sample_rate: 48_000,
            max_chunk_frames: 1,
            max_total_frames: 1,
        };
        let mut writer = JournalWriter::create(&journal, config).unwrap();
        writer.append(&[0.25, -0.25]).unwrap();
        drop(writer);
        fs::write(&replacement, b"foreign final data").unwrap();
        let mut journal_lock = open_locked(&journal).unwrap();
        let report = recover_file(&mut journal_lock).unwrap();

        let result = publish_locked_wav_observed(
            &journal,
            &mut journal_lock,
            &final_path,
            report,
            |_| {},
            |_| {
                fs::rename(&final_path, &linked_output).unwrap();
                fs::rename(&replacement, &final_path).unwrap();
            },
        );

        assert!(matches!(
            result,
            Err(JournalError::PublishedRecordingConflict(path)) if path == final_path
        ));
        assert_eq!(fs::read(&final_path).unwrap(), b"foreign final data");
        assert!(verify_wav_observed(&linked_output, report, || {}).is_some());
        fs::remove_dir_all(root).unwrap();
    }
}
