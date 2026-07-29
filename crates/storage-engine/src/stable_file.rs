use std::fs;
use std::io::{self, Read, Seek, SeekFrom};

use same_file::Handle;
use sha2::{Digest, Sha256};

use crate::{StorageError, StorageErrorKind, io_error, rejection};

const READ_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub(crate) struct StableFile {
    pub bytes: Option<Vec<u8>>,
}

pub(crate) fn read_open_stable(
    file: fs::File,
    label: &str,
    maximum_bytes: u64,
) -> Result<Vec<u8>, StorageError> {
    let verified = read_open_file(file, label, maximum_bytes, None, true)?;
    verified
        .bytes
        .ok_or_else(|| rejection(StorageErrorKind::Io, label, "file bytes are unavailable"))
}

pub(crate) fn verify_open_stable(
    file: fs::File,
    label: &str,
    expected_bytes: u64,
    expected_sha256: [u8; 32],
    capture: bool,
) -> Result<StableFile, StorageError> {
    read_open_file(
        file,
        label,
        expected_bytes,
        Some((expected_bytes, expected_sha256)),
        capture,
    )
}

fn read_open_file_observed<F>(
    file: fs::File,
    label: &str,
    maximum_bytes: u64,
    expected: Option<(u64, [u8; 32])>,
    capture: bool,
    after_read: F,
) -> Result<StableFile, StorageError>
where
    F: FnOnce() -> io::Result<()>,
{
    let handle = Handle::from_file(file).map_err(|error| io_error(label, "open file", error))?;
    let before = handle
        .as_file()
        .metadata()
        .map_err(|error| io_error(label, "inspect open file", error))?;
    require_regular_non_link(&before, label)?;
    if before.len() > maximum_bytes {
        return Err(rejection(
            StorageErrorKind::ByteLengthMismatch,
            label,
            "file exceeds its byte limit",
        ));
    }
    if let Some((expected_bytes, _)) = expected
        && before.len() != expected_bytes
    {
        return Err(rejection(
            StorageErrorKind::ByteLengthMismatch,
            label,
            "declared byte length does not match the file",
        ));
    }
    read_retained_handle(
        handle,
        &before,
        label,
        maximum_bytes,
        expected,
        capture,
        after_read,
    )
}

fn read_open_file(
    file: fs::File,
    label: &str,
    maximum_bytes: u64,
    expected: Option<(u64, [u8; 32])>,
    capture: bool,
) -> Result<StableFile, StorageError> {
    read_open_file_observed(file, label, maximum_bytes, expected, capture, || Ok(()))
}

fn read_retained_handle<F>(
    mut handle: Handle,
    before: &fs::Metadata,
    label: &str,
    maximum_bytes: u64,
    expected: Option<(u64, [u8; 32])>,
    capture: bool,
    after_read: F,
) -> Result<StableFile, StorageError>
where
    F: FnOnce() -> io::Result<()>,
{
    let before_stamp = metadata_stamp(before);
    let mut bytes = if capture {
        let capacity = usize::try_from(before.len()).map_err(|_| {
            rejection(
                StorageErrorKind::LimitExceeded,
                label,
                "file cannot fit in the process address space",
            )
        })?;
        let mut output = Vec::new();
        output.try_reserve_exact(capacity).map_err(|_| {
            rejection(
                StorageErrorKind::LimitExceeded,
                label,
                "file capture capacity is unavailable",
            )
        })?;
        Some(output)
    } else {
        None
    };
    let mut buffer = vec![0_u8; READ_BUFFER_BYTES].into_boxed_slice();
    let (sha256, total) = read_pass(
        handle.as_file_mut(),
        label,
        maximum_bytes,
        &mut buffer,
        bytes.as_mut(),
    )?;
    after_read().map_err(|error| io_error(label, "run read observer", error))?;
    handle
        .as_file_mut()
        .seek(SeekFrom::Start(0))
        .map_err(|error| io_error(label, "rewind file", error))?;
    let (confirmed_sha256, confirmed_total) = read_pass(
        handle.as_file_mut(),
        label,
        maximum_bytes,
        &mut buffer,
        None,
    )?;
    if sha256 != confirmed_sha256 || total != confirmed_total {
        return Err(changed(label));
    }

    let after = handle
        .as_file()
        .metadata()
        .map_err(|error| io_error(label, "reinspect open file", error))?;
    if !after.is_file()
        || confirmed_total != after.len()
        || before.len() != after.len()
        || before_stamp != metadata_stamp(&after)
    {
        return Err(changed(label));
    }

    if let Some((_, expected_sha256)) = expected
        && sha256 != expected_sha256
    {
        return Err(rejection(
            StorageErrorKind::Sha256Mismatch,
            label,
            "declared SHA-256 does not match the file",
        ));
    }
    Ok(StableFile { bytes })
}

fn read_pass(
    file: &mut fs::File,
    label: &str,
    maximum_bytes: u64,
    buffer: &mut [u8],
    mut capture: Option<&mut Vec<u8>>,
) -> Result<([u8; 32], u64), StorageError> {
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    loop {
        let count = file
            .read(buffer)
            .map_err(|error| io_error(label, "read file", error))?;
        if count == 0 {
            break;
        }
        total = total.checked_add(count as u64).ok_or_else(|| {
            rejection(
                StorageErrorKind::LimitExceeded,
                label,
                "file byte count overflowed",
            )
        })?;
        if total > maximum_bytes {
            return Err(rejection(
                StorageErrorKind::ByteLengthMismatch,
                label,
                "file grew beyond its byte limit while reading",
            ));
        }
        hasher.update(&buffer[..count]);
        if let Some(output) = &mut capture {
            output.extend_from_slice(&buffer[..count]);
        }
    }
    Ok((hasher.finalize().into(), total))
}

fn require_regular_non_link(metadata: &fs::Metadata, label: &str) -> Result<(), StorageError> {
    if is_link_like(metadata) {
        return Err(rejection(
            StorageErrorKind::LinkForbidden,
            label,
            "symbolic links, junctions, and reparse points are forbidden",
        ));
    }
    if !metadata.is_file() {
        return Err(rejection(
            StorageErrorKind::UnsupportedEntry,
            label,
            "path must identify a regular file",
        ));
    }
    Ok(())
}

fn changed(label: &str) -> StorageError {
    rejection(
        StorageErrorKind::ChangedDuringVerification,
        label,
        "file or its path changed during verification",
    )
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

#[cfg(not(any(unix, windows)))]
compile_error!("hyper-storage-engine supports only Unix and Windows targets");

#[cfg(unix)]
pub(crate) fn is_link_like(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
pub(crate) fn is_link_like(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[allow(dead_code)]
fn _assert_io_error_is_send_sync(_: io::Error) {}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn rejects_a_file_changed_after_its_bytes_are_read() {
        let root = std::env::temp_dir().join(format!(
            "hyper-stable-file-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let path = root.join("payload");
        fs::write(&path, b"original").unwrap();
        let file = fs::File::open(&path).unwrap();
        let result = read_open_file_observed(file, "payload", 8, None, true, || {
            fs::write(&path, b"changed!")
        });
        assert_eq!(
            result.unwrap_err().kind(),
            StorageErrorKind::ChangedDuringVerification
        );
        fs::remove_dir_all(root).unwrap();
    }
}
