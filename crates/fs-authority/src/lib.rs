use std::error::Error;
use std::ffi::OsString;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use cap_fs_ext::{DirExt, FollowSymlinks, MetadataExt, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, DirBuilder, File, Metadata, OpenOptions};
use sha2::{Digest, Sha256};

#[cfg(target_os = "linux")]
use std::fs::File as StdFile;

#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;

#[cfg(target_os = "linux")]
use rustix::fs::{AtFlags, Mode, OFlags};

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_rename {
    use std::ffi::OsStr;
    use std::io;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use std::ptr::{addr_of_mut, copy_nonoverlapping};

    use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
    use cap_std::fs::{Dir, File, OpenOptions, OpenOptionsExt};
    use windows_sys::Wdk::Storage::FileSystem::{
        FILE_DISPOSITION_INFORMATION, FILE_RENAME_INFORMATION, FileDispositionInformation,
        FileRenameInformation, NtSetInformationFile,
    };
    use windows_sys::Win32::Foundation::RtlNtStatusToDosError;
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    use super::{identity, invalid_argument};

    fn open_verified_source(
        source_directory: &Dir,
        source: &str,
        expected_identity: (u64, u64),
    ) -> io::Result<File> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .access_mode(DELETE | FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .follow(FollowSymlinks::No);
        let source_handle = source_directory.open_with(source, &options)?;
        if identity(&source_handle.metadata()?) != expected_identity {
            return Err(invalid_argument(
                "source entry identity does not match retained authority",
            ));
        }
        Ok(source_handle)
    }

    fn rename_open_source(
        source_handle: &File,
        destination_directory: &Dir,
        destination: &str,
        replace: bool,
    ) -> io::Result<()> {
        let mut destination_name: Vec<u16> = OsStr::new(destination).encode_wide().collect();
        let name_bytes = destination_name
            .len()
            .checked_mul(size_of::<u16>())
            .and_then(|length| u32::try_from(length).ok())
            .ok_or_else(|| invalid_argument("destination entry name is too long"))?;
        destination_name.push(0);
        let buffer_bytes = size_of::<FILE_RENAME_INFORMATION>()
            .checked_add(name_bytes as usize)
            .ok_or_else(|| invalid_argument("rename information size overflow"))?;
        let storage_words = buffer_bytes.div_ceil(size_of::<usize>());
        let mut storage = vec![0_usize; storage_words];
        let information = storage.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
        let mut io_status = IO_STATUS_BLOCK::default();

        // SAFETY: `storage` is aligned to `usize`, sized for the fixed header and
        // complete UTF-16 name, and remains alive for the duration of the call.
        // Both handles are owned Rust values. The source was opened with DELETE
        // access, and the destination name is resolved relative to the retained
        // destination directory handle.
        let status = unsafe {
            (*information).Anonymous.ReplaceIfExists = replace;
            (*information).RootDirectory = destination_directory.as_raw_handle();
            (*information).FileNameLength = name_bytes;
            copy_nonoverlapping(
                destination_name.as_ptr(),
                addr_of_mut!((*information).FileName).cast::<u16>(),
                destination_name.len(),
            );
            NtSetInformationFile(
                source_handle.as_raw_handle(),
                &raw mut io_status,
                information.cast(),
                u32::try_from(buffer_bytes)
                    .map_err(|_| invalid_argument("rename information is too large"))?,
                FileRenameInformation,
            )
        };
        if status >= 0 {
            return Ok(());
        }
        // SAFETY: `status` was returned by an NT system service and is passed
        // directly to the documented status-to-Win32 conversion routine.
        let error = unsafe { RtlNtStatusToDosError(status) };
        Err(io::Error::from_raw_os_error(
            i32::try_from(error).unwrap_or(i32::MAX),
        ))
    }

    fn remove_open_source(source_handle: &File) -> io::Result<()> {
        let mut disposition = FILE_DISPOSITION_INFORMATION { DeleteFile: true };
        let mut io_status = IO_STATUS_BLOCK::default();
        let status = unsafe {
            NtSetInformationFile(
                source_handle.as_raw_handle(),
                &raw mut io_status,
                (&raw mut disposition).cast(),
                u32::try_from(size_of::<FILE_DISPOSITION_INFORMATION>())
                    .map_err(|_| invalid_argument("file disposition information is too large"))?,
                FileDispositionInformation,
            )
        };
        if status >= 0 {
            return Ok(());
        }
        let error = unsafe { RtlNtStatusToDosError(status) };
        Err(io::Error::from_raw_os_error(
            i32::try_from(error).unwrap_or(i32::MAX),
        ))
    }

    pub(super) fn rename(
        source_directory: &Dir,
        source: &str,
        expected_identity: (u64, u64),
        destination_directory: &Dir,
        destination: &str,
        replace: bool,
    ) -> io::Result<()> {
        let source_handle = open_verified_source(source_directory, source, expected_identity)?;
        rename_open_source(&source_handle, destination_directory, destination, replace)
    }

    pub(super) fn remove(
        directory: &Dir,
        name: &str,
        expected_identity: (u64, u64),
    ) -> io::Result<()> {
        let source_handle = open_verified_source(directory, name, expected_identity)?;
        remove_open_source(&source_handle)
    }

    #[cfg(test)]
    mod tests {
        use std::fs;
        use std::path::{Path, PathBuf};
        use std::time::{SystemTime, UNIX_EPOCH};

        use cap_std::ambient_authority;
        use cap_std::fs::Dir;

        use super::{open_verified_source, rename_open_source};
        use crate::identity;

        struct TemporaryDirectory(PathBuf);

        impl TemporaryDirectory {
            fn new() -> Self {
                let nonce = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("system clock must follow the Unix epoch")
                    .as_nanos();
                let path = std::env::temp_dir().join(format!(
                    "hyper-fs-authority-rename-{}-{nonce}",
                    std::process::id()
                ));
                fs::create_dir(&path).expect("temporary authority root must be unique");
                Self(path)
            }

            fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for TemporaryDirectory {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        #[test]
        fn retained_destination_is_immutable_and_resolved_by_handle() {
            let temporary = TemporaryDirectory::new();
            let source_parent_path = temporary.path().join("source");
            let destination_path = temporary.path().join("destination");
            let retained_destination_path = temporary.path().join("retained-destination");
            fs::create_dir(&source_parent_path).unwrap();
            fs::create_dir(&destination_path).unwrap();
            fs::create_dir(source_parent_path.join("entry")).unwrap();
            fs::write(source_parent_path.join("entry/marker"), b"retained").unwrap();

            let source_parent =
                Dir::open_ambient_dir(&source_parent_path, ambient_authority()).unwrap();
            let destination =
                Dir::open_ambient_dir(&destination_path, ambient_authority()).unwrap();
            let source_identity = identity(&source_parent.symlink_metadata("entry").unwrap());
            let source = open_verified_source(&source_parent, "entry", source_identity).unwrap();

            let displacement = fs::rename(&destination_path, &retained_destination_path)
                .expect_err("retained Windows directories must reject ambient displacement");
            assert_eq!(displacement.raw_os_error(), Some(32));

            rename_open_source(&source, &destination, "published", false).unwrap();

            assert_eq!(
                fs::read(destination_path.join("published/marker")).unwrap(),
                b"retained"
            );
            assert!(!retained_destination_path.exists());
        }
    }
}

fn invalid_argument(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

/// Returns the platform filesystem identity used by retained-authority checks.
#[must_use]
pub fn identity(metadata: &Metadata) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
}

/// Returns whether an absolute path is proven absent while refusing to follow
/// links in any traversed component.
///
/// An existing link, irregular entry, access failure, or unstable namespace is
/// not absence and is returned as an error. A missing component below a retained
/// real directory is proof that the complete path cannot currently exist.
///
/// # Errors
///
/// Returns an I/O or validation error when the path is not absolute, contains
/// lexical aliases, crosses a link, or cannot be classified through retained
/// directory authority.
pub fn path_is_proven_absent(path: &Path) -> io::Result<bool> {
    if !path.is_absolute() {
        return Err(invalid_argument("classified path must be absolute"));
    }
    let mut anchor = PathBuf::new();
    let mut names = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => anchor.push(component.as_os_str()),
            Component::Normal(name) => names.push(name),
            Component::CurDir | Component::ParentDir => {
                return Err(invalid_argument(
                    "classified path must not contain lexical aliases",
                ));
            }
        }
    }
    if anchor.as_os_str().is_empty() || names.is_empty() {
        return Err(invalid_argument(
            "classified path must name an entry below an absolute root",
        ));
    }
    let mut directory = Dir::open_ambient_dir(anchor, ambient_authority())?;
    for (index, name) in names.iter().enumerate() {
        let metadata = match directory.symlink_metadata(name) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
            Err(error) => return Err(error),
        };
        if metadata.file_type().is_symlink() {
            return Err(invalid_argument("classified path crosses a link"));
        }
        if index + 1 == names.len() {
            return Ok(false);
        }
        if !metadata.is_dir() {
            return Err(invalid_argument(
                "classified path crosses a non-directory entry",
            ));
        }
        directory = directory.open_dir_nofollow(name)?;
    }
    Ok(false)
}

/// Reads one bounded regular non-link file while retaining its identity and
/// proving that two complete reads and the published directory entry agree.
///
/// # Errors
///
/// Returns an I/O or validation error when the entry is linked, non-regular,
/// oversized, replaced, or changes while it is read.
pub fn read_stable_file(directory: &Dir, name: &str, maximum_bytes: u64) -> io::Result<Vec<u8>> {
    read_stable_file_observed(directory, name, maximum_bytes, || Ok(()))
}

/// Identity and content evidence for one stable external file snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StableFileEvidence {
    identity: (u64, u64),
    byte_length: u64,
    sha256: [u8; 32],
}

impl StableFileEvidence {
    /// Returns the retained filesystem identity of the verified directory entry.
    #[must_use]
    pub const fn identity(&self) -> (u64, u64) {
        self.identity
    }
}

/// Exact bytes and evidence captured from one stable external file.
#[derive(Debug, Eq, PartialEq)]
pub struct StableFileSnapshot {
    bytes: Vec<u8>,
    evidence: StableFileEvidence,
}

impl StableFileSnapshot {
    #[must_use]
    pub fn into_parts(self) -> (Vec<u8>, StableFileEvidence) {
        (self.bytes, self.evidence)
    }
}

/// Reads one bounded regular non-link file while retaining the exact bytes and
/// the identity evidence observed during that same stable read.
///
/// # Errors
///
/// Returns an I/O or validation error when the entry is linked, non-regular,
/// oversized, replaced, or changes while it is read.
pub fn read_stable_file_snapshot(
    directory: &Dir,
    name: &str,
    maximum_bytes: u64,
) -> io::Result<StableFileSnapshot> {
    read_stable_file_snapshot_observed(directory, Path::new(name), maximum_bytes, || Ok(()))
}

/// Reads one absolute regular file without following any path link, proving
/// that two complete reads and the published entry agree.
///
/// # Errors
///
/// Returns an I/O or validation error when the path is not absolute, contains
/// lexical aliases, crosses a link, names an irregular entry, exceeds the byte
/// limit, is replaced, or changes while being read.
pub fn read_stable_absolute_file(
    path: &Path,
    maximum_bytes: u64,
) -> io::Result<StableFileSnapshot> {
    let (parent, name) = open_absolute_parent(path)?;
    read_stable_file_snapshot_observed(&parent, &name, maximum_bytes, || Ok(()))
}

/// Confirms that an absolute external file still has the captured filesystem
/// identity and exact content.
///
/// # Errors
///
/// Returns an I/O or validation error when the current entry is inaccessible,
/// linked, irregular, oversized, unstable, replaced, or content-changed.
pub fn confirm_stable_absolute_file(
    path: &Path,
    maximum_bytes: u64,
    expected: &StableFileEvidence,
) -> io::Result<()> {
    let current = read_stable_absolute_file(path, maximum_bytes)?;
    if current.evidence != *expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "stable file identity or content changed",
        ));
    }
    Ok(())
}

fn read_stable_file_observed(
    directory: &Dir,
    name: &str,
    maximum_bytes: u64,
    after_first_read: impl FnOnce() -> io::Result<()>,
) -> io::Result<Vec<u8>> {
    read_stable_file_snapshot_observed(
        directory,
        Path::new(name),
        maximum_bytes,
        after_first_read,
    )
    .map(|snapshot| snapshot.bytes)
}

fn read_stable_file_snapshot_observed(
    directory: &Dir,
    name: &Path,
    maximum_bytes: u64,
    after_first_read: impl FnOnce() -> io::Result<()>,
) -> io::Result<StableFileSnapshot> {
    let published = directory.symlink_metadata(name)?;
    if published.file_type().is_symlink() || !published.is_file() {
        return Err(invalid_argument(
            "stable file must be a regular non-link file",
        ));
    }
    if published.len() > maximum_bytes {
        return Err(invalid_argument("stable file exceeds its byte limit"));
    }
    let published_identity = identity(&published);
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let mut file = directory.open_with(name, &options)?;
    let before = file.metadata()?;
    if !before.is_file()
        || before.len() != published.len()
        || identity(&before) != published_identity
    {
        return Err(invalid_argument(
            "stable file identity changed before reading",
        ));
    }
    let before_stamp = metadata_stamp(&file)?;
    let first = read_bounded(&mut file, before.len(), maximum_bytes)?;
    after_first_read()?;
    file.seek(SeekFrom::Start(0))?;
    let second_matches = read_matches(&mut file, &first)?;
    let after = file.metadata()?;
    let republished = directory.symlink_metadata(name)?;
    if !second_matches
        || !after.is_file()
        || after.len() != first.len() as u64
        || identity(&after) != published_identity
        || metadata_stamp(&file)? != before_stamp
        || republished.file_type().is_symlink()
        || !republished.is_file()
        || identity(&republished) != published_identity
    {
        return Err(invalid_argument("stable file changed while being read"));
    }
    Ok(StableFileSnapshot {
        evidence: StableFileEvidence {
            identity: published_identity,
            byte_length: first.len() as u64,
            sha256: Sha256::digest(&first).into(),
        },
        bytes: first,
    })
}

fn read_matches(file: &mut File, expected: &[u8]) -> io::Result<bool> {
    let mut offset = 0;
    let mut buffer = [0_u8; 8 * 1_024];
    while offset < expected.len() {
        let count = (expected.len() - offset).min(buffer.len());
        file.read_exact(&mut buffer[..count])?;
        if buffer[..count] != expected[offset..offset + count] {
            return Ok(false);
        }
        offset += count;
    }
    let mut trailing = [0_u8; 1];
    Ok(file.read(&mut trailing)? == 0)
}

fn open_absolute_parent(path: &Path) -> io::Result<(Dir, PathBuf)> {
    if !path.is_absolute() {
        return Err(invalid_argument("stable file path must be absolute"));
    }
    let mut anchor = PathBuf::new();
    let mut names = Vec::<OsString>::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => anchor.push(component.as_os_str()),
            Component::Normal(name) => names.push(name.to_owned()),
            Component::CurDir | Component::ParentDir => {
                return Err(invalid_argument(
                    "stable file path must not contain lexical aliases",
                ));
            }
        }
    }
    let leaf = names
        .pop()
        .ok_or_else(|| invalid_argument("stable file path must name an entry"))?;
    let mut directory = Dir::open_ambient_dir(anchor, ambient_authority())?;
    for name in names {
        let metadata = directory.symlink_metadata(&name)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(invalid_argument(
                "stable file path crosses a link or non-directory entry",
            ));
        }
        directory = directory.open_dir_nofollow(&name)?;
    }
    Ok((directory, PathBuf::from(leaf)))
}

#[cfg(unix)]
fn metadata_stamp(file: &File) -> io::Result<(i64, i64, i64, i64)> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.try_clone()?.into_std().metadata()?;
    Ok((
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    ))
}

#[cfg(windows)]
fn metadata_stamp(file: &File) -> io::Result<(u64, u64, u32)> {
    use std::os::windows::fs::MetadataExt;

    let metadata = file.try_clone()?.into_std().metadata()?;
    Ok((
        metadata.last_write_time(),
        metadata.creation_time(),
        metadata.file_attributes(),
    ))
}

/// Synchronizes a retained directory authority.
///
/// # Errors
///
/// Returns the platform synchronization failure.
pub fn sync_directory(directory: &Dir) -> io::Result<()> {
    directory.try_clone()?.into_std_file().sync_all()
}

/// Opens an ambient directory with the access required for an explicit
/// metadata durability barrier on every supported desktop platform.
///
/// # Errors
///
/// Returns an I/O error when the directory cannot be opened for durable
/// publication.
pub fn open_syncable_directory_ambient(path: &Path) -> io::Result<Dir> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;

        use rustix::fs::OFlags;

        let validated = Dir::open_ambient_dir(path, ambient_authority())?;
        let expected_identity = identity(&validated.dir_metadata()?);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags((OFlags::DIRECTORY | OFlags::NOFOLLOW).bits().cast_signed())
            .open(path)?;
        let directory = Dir::from_std_file(file);
        let opened = directory.dir_metadata()?;
        if !opened.is_dir() || identity(&opened) != expected_identity {
            return Err(invalid_argument(
                "syncable directory identity does not match validated authority",
            ));
        }
        Ok(directory)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        const FILE_SHARE_READ: u32 = 1;
        const FILE_SHARE_WRITE: u32 = 2;
        const FILE_SHARE_DELETE: u32 = 4;
        let validated = Dir::open_ambient_dir(path, ambient_authority())?;
        let expected_identity = identity(&validated.dir_metadata()?);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let directory = Dir::from_std_file(file);
        let opened = directory.dir_metadata()?;
        if !opened.is_dir() || identity(&opened) != expected_identity {
            return Err(invalid_argument(
                "syncable directory identity does not match validated authority",
            ));
        }
        Ok(directory)
    }
}

/// Opens a non-link child directory with the access required for an explicit
/// metadata durability barrier.
///
/// # Errors
///
/// Returns an I/O or validation error when the child cannot be opened as a
/// real directory with synchronization access.
pub fn open_syncable_directory_at(parent: &Dir, name: &str) -> io::Result<Dir> {
    #[cfg(target_os = "linux")]
    {
        let validated = parent.open_dir_nofollow(name)?;
        let expected_identity = identity(&validated.dir_metadata()?);
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let file = parent.open_with(name, &options)?;
        let metadata = file.metadata()?;
        if !metadata.is_dir() || identity(&metadata) != expected_identity {
            return Err(invalid_argument(
                "syncable directory identity does not match validated authority",
            ));
        }
        Ok(Dir::from_std_file(file.into_std()))
    }
    #[cfg(windows)]
    {
        use cap_std::fs::OpenOptionsExt;

        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        const FILE_SHARE_READ: u32 = 1;
        const FILE_SHARE_WRITE: u32 = 2;
        const FILE_SHARE_DELETE: u32 = 4;
        let validated = parent.open_dir_nofollow(name)?;
        let expected_identity = identity(&validated.dir_metadata()?);
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .follow(FollowSymlinks::No);
        let file = parent.open_with(name, &options)?;
        let metadata = file.metadata()?;
        if !metadata.is_dir() || identity(&metadata) != expected_identity {
            return Err(invalid_argument(
                "syncable directory identity does not match validated authority",
            ));
        }
        Ok(Dir::from_std_file(file.into_std()))
    }
}

fn read_bounded(file: &mut File, expected_bytes: u64, maximum_bytes: u64) -> io::Result<Vec<u8>> {
    let capacity = usize::try_from(expected_bytes)
        .map_err(|_| invalid_argument("stable file cannot fit in memory"))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| invalid_argument("stable file capacity is unavailable"))?;
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let next = u64::try_from(bytes.len())
            .unwrap_or(u64::MAX)
            .checked_add(count as u64)
            .ok_or_else(|| invalid_argument("stable file byte count overflowed"))?;
        if next > maximum_bytes {
            return Err(invalid_argument("stable file grew beyond its byte limit"));
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    Ok(bytes)
}

fn parse_identity(value: Option<String>, message: &'static str) -> Result<u64, Box<dyn Error>> {
    Ok(value
        .ok_or_else(|| invalid_argument(message))?
        .parse::<u64>()?)
}

fn parse_sha256(
    value: Option<String>,
    message: &'static str,
    optional: bool,
) -> Result<Option<String>, Box<dyn Error>> {
    let value = value.ok_or_else(|| invalid_argument(message))?;
    if optional && value == "-" {
        return Ok(None);
    }
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid_argument("SHA-256 must be 64 lowercase hexadecimal digits").into());
    }
    Ok(Some(value))
}

fn parse_relative_path(
    value: Option<String>,
    message: &'static str,
) -> Result<Vec<String>, Box<dyn Error>> {
    let value = value.ok_or_else(|| invalid_argument(message))?;
    let components: Vec<_> = value.split('/').map(str::to_owned).collect();
    if components.is_empty()
        || components
            .iter()
            .any(|component| component.is_empty() || component == "." || component == "..")
        || value.contains('\\')
    {
        return Err(invalid_argument(
            "filesystem relative path must contain only normal POSIX components",
        )
        .into());
    }
    Ok(components)
}

fn parse_name(value: Option<String>, message: &'static str) -> Result<String, Box<dyn Error>> {
    let value = value.ok_or_else(|| invalid_argument(message))?;
    let mut components = Path::new(&value).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(invalid_argument("filesystem entry name must be one normal component").into());
    }
    Ok(value)
}

/// Opens a directory without accepting a different filesystem object at the
/// ambient path.
///
/// # Errors
///
/// Returns an I/O or validation error when the path cannot be opened as the
/// expected directory identity.
pub fn open_verified_directory(
    path: &str,
    expected_identity: (u64, u64),
) -> Result<Dir, Box<dyn Error>> {
    let directory = Dir::open_ambient_dir(Path::new(path), ambient_authority())?;
    let metadata = directory.dir_metadata()?;
    if !metadata.is_dir() || identity(&metadata) != expected_identity {
        return Err(
            invalid_argument("directory identity does not match retained authority").into(),
        );
    }
    Ok(directory)
}

#[cfg(target_os = "linux")]
fn open_copy_source(path: &str) -> io::Result<File> {
    File::open_ambient(path, ambient_authority())
}

#[cfg(windows)]
fn open_copy_source(path: &str) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    File::open_ambient_with(path, &options, ambient_authority())
}

fn create_destination_directory(parent: &Dir, name: &str) -> io::Result<()> {
    #[cfg(unix)]
    let builder = {
        use cap_std::fs::DirBuilderExt;
        let mut builder = DirBuilder::new();
        builder.mode(0o755);
        builder
    };
    #[cfg(not(unix))]
    let builder = DirBuilder::new();
    match parent.create_dir_with(name, &builder) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

fn create_verified_directory(
    arguments: &mut impl Iterator<Item = String>,
) -> Result<(), Box<dyn Error>> {
    let parent = arguments
        .next()
        .ok_or_else(|| invalid_argument("parent directory path is required"))?;
    let expected_device = parse_identity(arguments.next(), "parent device identity is required")?;
    let expected_file = parse_identity(arguments.next(), "parent file identity is required")?;
    let name = parse_name(arguments.next(), "directory name is required")?;
    if arguments.next().is_some() {
        return Err(invalid_argument("unexpected filesystem authority argument").into());
    }

    let parent = open_verified_directory(&parent, (expected_device, expected_file))?;
    #[cfg(unix)]
    let builder = {
        use cap_std::fs::DirBuilderExt;
        let mut builder = DirBuilder::new();
        builder.mode(0o700);
        builder
    };
    #[cfg(not(unix))]
    let builder = DirBuilder::new();
    parent.create_dir_with(&name, &builder)?;
    let directory = parent.open_dir_nofollow(&name)?;
    let metadata = directory.dir_metadata()?;
    if !metadata.is_dir() {
        return Err(invalid_argument("created entry is not a directory").into());
    }
    let (device, file) = identity(&metadata);
    println!("{device} {file}");
    Ok(())
}

/// Removes an identity-bound child directory without following replacement
/// entries.
///
/// # Errors
///
/// Returns an I/O or validation error when the published child is not the
/// expected retained directory identity.
pub fn remove_directory_entry(
    parent: &Dir,
    name: &str,
    expected_identity: (u64, u64),
) -> io::Result<()> {
    let directory = parent.open_dir_nofollow(name)?;
    let metadata = directory.dir_metadata()?;
    if !metadata.is_dir() || identity(&metadata) != expected_identity {
        return Err(invalid_argument(
            "directory entry identity does not match retained authority",
        ));
    }
    directory.remove_open_dir_all()
}

fn remove_verified_directory_entry(
    arguments: &mut impl Iterator<Item = String>,
) -> Result<(), Box<dyn Error>> {
    let parent = arguments
        .next()
        .ok_or_else(|| invalid_argument("parent directory path is required"))?;
    let expected_device = parse_identity(arguments.next(), "parent device identity is required")?;
    let expected_file = parse_identity(arguments.next(), "parent file identity is required")?;
    let name = parse_name(arguments.next(), "directory name is required")?;
    let entry_device = parse_identity(arguments.next(), "directory device identity is required")?;
    let entry_file = parse_identity(arguments.next(), "directory file identity is required")?;
    if arguments.next().is_some() {
        return Err(invalid_argument("unexpected filesystem authority argument").into());
    }

    let parent = open_verified_directory(&parent, (expected_device, expected_file))?;
    remove_directory_entry(&parent, &name, (entry_device, entry_file))?;
    Ok(())
}

fn open_destination_parent(
    destination_root: &Dir,
    components: &[String],
) -> io::Result<(Dir, String)> {
    let (file_name, parents) = components
        .split_last()
        .ok_or_else(|| invalid_argument("destination path is empty"))?;
    let mut current = destination_root.try_clone()?;
    for component in parents {
        create_destination_directory(&current, component)?;
        current = current.open_dir_nofollow(component)?;
    }
    Ok((current, file_name.clone()))
}

fn copy_to_destination(
    reader: &mut impl Read,
    destination_root: &Dir,
    destination_components: &[String],
    expected_bytes: u64,
    expected_sha256: Option<&str>,
    maximum_bytes: u64,
) -> Result<(u64, String), Box<dyn Error>> {
    if expected_bytes > maximum_bytes {
        return Err(invalid_argument("copy exceeds its remaining byte budget").into());
    }
    let (destination_parent, destination_name) =
        open_destination_parent(destination_root, destination_components)?;
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.mode(0o644);
    }
    let mut destination = destination_parent.open_with(destination_name, &options)?;
    let mut digest = Sha256::new();
    let mut copied = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        copied = copied
            .checked_add(read as u64)
            .ok_or_else(|| invalid_argument("copy byte count overflow"))?;
        if copied > expected_bytes || copied > maximum_bytes {
            return Err(invalid_argument("copy source exceeded its byte budget").into());
        }
        digest.update(&buffer[..read]);
        destination.write_all(&buffer[..read])?;
    }
    if copied != expected_bytes {
        return Err(
            invalid_argument("copy source byte length does not match its descriptor").into(),
        );
    }
    let observed_sha256 = format!("{:x}", digest.finalize());
    if expected_sha256.is_some_and(|expected| expected != observed_sha256) {
        return Err(invalid_argument("copy source SHA-256 does not match its descriptor").into());
    }
    #[cfg(unix)]
    {
        use cap_std::fs::{Permissions, PermissionsExt};
        destination.set_permissions(Permissions::from_mode(0o644))?;
    }
    destination.sync_all()?;
    Ok((copied, observed_sha256))
}

fn copy_verified_file(arguments: &mut impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let source_path = arguments
        .next()
        .ok_or_else(|| invalid_argument("copy source path is required"))?;
    let source_device = parse_identity(arguments.next(), "copy source device is required")?;
    let source_file = parse_identity(arguments.next(), "copy source file is required")?;
    let destination_root = arguments
        .next()
        .ok_or_else(|| invalid_argument("copy destination root is required"))?;
    let destination_device =
        parse_identity(arguments.next(), "copy destination device is required")?;
    let destination_file = parse_identity(arguments.next(), "copy destination file is required")?;
    let destination_path =
        parse_relative_path(arguments.next(), "copy destination path is required")?;
    let expected_bytes = parse_identity(arguments.next(), "copy byte length is required")?;
    let expected_sha256 = parse_sha256(arguments.next(), "copy SHA-256 is required", true)?;
    let maximum_bytes = parse_identity(arguments.next(), "copy byte budget is required")?;
    if arguments.next().is_some() {
        return Err(invalid_argument("unexpected filesystem authority argument").into());
    }

    let mut source = open_copy_source(&source_path)?;
    let before = source.metadata()?;
    if !before.is_file()
        || identity(&before) != (source_device, source_file)
        || before.len() != expected_bytes
    {
        return Err(
            invalid_argument("copy source identity does not match retained authority").into(),
        );
    }
    let before_modified = before.modified()?;
    let destination =
        open_verified_directory(&destination_root, (destination_device, destination_file))?;
    let (copied, observed_sha256) = copy_to_destination(
        &mut source,
        &destination,
        &destination_path,
        expected_bytes,
        expected_sha256.as_deref(),
        maximum_bytes,
    )?;
    let after = source.metadata()?;
    if identity(&after) != identity(&before)
        || after.len() != before.len()
        || after.modified()? != before_modified
    {
        return Err(invalid_argument("copy source changed during transfer").into());
    }
    println!("{copied} {observed_sha256}");
    Ok(())
}

fn write_verified_file(arguments: &mut impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let destination_root = arguments
        .next()
        .ok_or_else(|| invalid_argument("write destination root is required"))?;
    let destination_device =
        parse_identity(arguments.next(), "write destination device is required")?;
    let destination_file = parse_identity(arguments.next(), "write destination file is required")?;
    let destination_path =
        parse_relative_path(arguments.next(), "write destination path is required")?;
    let expected_bytes = parse_identity(arguments.next(), "write byte length is required")?;
    let expected_sha256 = parse_sha256(arguments.next(), "write SHA-256 is required", false)?
        .expect("non-optional SHA-256 parser returned no value");
    let maximum_bytes = parse_identity(arguments.next(), "write byte budget is required")?;
    if arguments.next().is_some() {
        return Err(invalid_argument("unexpected filesystem authority argument").into());
    }

    let destination =
        open_verified_directory(&destination_root, (destination_device, destination_file))?;
    let (copied, observed_sha256) = copy_to_destination(
        &mut io::stdin().lock(),
        &destination,
        &destination_path,
        expected_bytes,
        Some(&expected_sha256),
        maximum_bytes,
    )?;
    println!("{copied} {observed_sha256}");
    Ok(())
}

fn remove_verified_directory(
    arguments: &mut impl Iterator<Item = String>,
) -> Result<(), Box<dyn Error>> {
    let path = arguments
        .next()
        .ok_or_else(|| invalid_argument("directory path is required"))?;
    let expected_device =
        parse_identity(arguments.next(), "directory device identity is required")?;
    let expected_file = parse_identity(arguments.next(), "directory file identity is required")?;
    if arguments.next().is_some() {
        return Err(invalid_argument("unexpected filesystem authority argument").into());
    }

    let directory = open_verified_directory(&path, (expected_device, expected_file))?;
    directory.remove_open_dir_all()?;
    Ok(())
}

#[cfg(target_os = "linux")]
/// Renames an identity-bound entry only when the destination does not exist.
///
/// # Errors
///
/// Returns an I/O or identity error when the source changed, the destination
/// exists, or the platform rename fails.
pub fn rename_no_replace(
    source_directory: &Dir,
    source: &str,
    expected_identity: (u64, u64),
    destination_directory: &Dir,
    destination: &str,
) -> io::Result<()> {
    let metadata = source_directory.symlink_metadata(source)?;
    if identity(&metadata) != expected_identity {
        return Err(invalid_argument(
            "source entry identity does not match retained authority",
        ));
    }
    Ok(rustix::fs::renameat_with(
        source_directory,
        source,
        destination_directory,
        destination,
        rustix::fs::RenameFlags::NOREPLACE,
    )?)
}

#[cfg(windows)]
/// Renames an identity-bound entry only when the destination does not exist.
///
/// # Errors
///
/// Returns an I/O or identity error when the source changed, the destination
/// exists, or the platform rename fails.
pub fn rename_no_replace(
    source_directory: &Dir,
    source: &str,
    expected_identity: (u64, u64),
    destination_directory: &Dir,
    destination: &str,
) -> io::Result<()> {
    windows_rename::rename(
        source_directory,
        source,
        expected_identity,
        destination_directory,
        destination,
        false,
    )
}

#[cfg(target_os = "linux")]
/// Atomically replaces a destination with an identity-bound source entry.
///
/// # Errors
///
/// Returns an I/O or identity error when the source changed or the platform
/// rename fails.
pub fn rename_replace(
    source_directory: &Dir,
    source: &str,
    expected_identity: (u64, u64),
    destination_directory: &Dir,
    destination: &str,
) -> io::Result<()> {
    let metadata = source_directory.symlink_metadata(source)?;
    if identity(&metadata) != expected_identity {
        return Err(invalid_argument(
            "source entry identity does not match retained authority",
        ));
    }
    source_directory.rename(source, destination_directory, destination)
}

#[cfg(windows)]
/// Atomically replaces a destination with an identity-bound source entry.
///
/// # Errors
///
/// Returns an I/O or identity error when the source changed or the platform
/// rename fails.
pub fn rename_replace(
    source_directory: &Dir,
    source: &str,
    expected_identity: (u64, u64),
    destination_directory: &Dir,
    destination: &str,
) -> io::Result<()> {
    windows_rename::rename(
        source_directory,
        source,
        expected_identity,
        destination_directory,
        destination,
        true,
    )
}

#[cfg(windows)]
/// Removes a regular entry only through an opened handle whose identity matches
/// the caller's retained authority.
///
/// # Errors
///
/// Returns an I/O or identity error when the entry changed or handle-bound
/// deletion fails. A replacement at the original pathname is never removed.
pub fn remove_file_if_identity(
    directory: &Dir,
    name: &str,
    expected_identity: (u64, u64),
) -> io::Result<()> {
    windows_rename::remove(directory, name, expected_identity)
}

#[cfg(target_os = "linux")]
/// Atomically publishes already-verified bytes to a destination which is
/// currently absent, without ever naming the temporary inode.
///
/// Linux cannot bind a source file descriptor to an overwrite rename. This
/// operation therefore fails with `AlreadyExists` instead of weakening the
/// verified-publication guarantee for an existing destination. It links the
/// unnamed inode through its `/proc/self/fd` capability path, which works for
/// ordinary users without the capability required by `AT_EMPTY_PATH`.
///
/// # Errors
///
/// Returns an I/O or validation error when the destination name is not one
/// normal component, the filesystem does not support unnamed temporary files,
/// writing or syncing fails, or the destination already exists.
pub fn publish_bytes_no_replace(
    directory: &Dir,
    destination: &str,
    bytes: &[u8],
) -> io::Result<()> {
    validate_entry_name(destination)?;
    let descriptor = rustix::fs::openat(
        directory,
        ".",
        OFlags::RDWR | OFlags::CLOEXEC | OFlags::TMPFILE,
        Mode::from_bits_truncate(0o600),
    )?;
    let mut temporary = StdFile::from(descriptor);
    temporary.write_all(bytes)?;
    temporary.sync_all()?;
    let source = format!("/proc/self/fd/{}", temporary.as_raw_fd());
    Ok(rustix::fs::linkat(
        rustix::fs::CWD,
        source,
        directory,
        destination,
        AtFlags::SYMLINK_FOLLOW,
    )?)
}

#[cfg(target_os = "linux")]
fn validate_entry_name(name: &str) -> io::Result<()> {
    let mut components = Path::new(name).components();
    if matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none() {
        return Ok(());
    }
    Err(invalid_argument(
        "filesystem entry name must be one normal component",
    ))
}

fn rename_verified_directory_no_replace(
    arguments: &mut impl Iterator<Item = String>,
) -> Result<(), Box<dyn Error>> {
    let parent = arguments
        .next()
        .ok_or_else(|| invalid_argument("parent directory path is required"))?;
    let expected_device = parse_identity(arguments.next(), "parent device identity is required")?;
    let expected_file = parse_identity(arguments.next(), "parent file identity is required")?;
    let source = parse_name(arguments.next(), "source directory name is required")?;
    let source_device = parse_identity(arguments.next(), "source device identity is required")?;
    let source_file = parse_identity(arguments.next(), "source file identity is required")?;
    let destination = parse_name(arguments.next(), "destination directory name is required")?;
    if arguments.next().is_some() {
        return Err(invalid_argument("unexpected filesystem authority argument").into());
    }

    let directory = open_verified_directory(&parent, (expected_device, expected_file))?;
    rename_no_replace(
        &directory,
        &source,
        (source_device, source_file),
        &directory,
        &destination,
    )?;
    Ok(())
}

fn rename_verified_directory_between_no_replace(
    arguments: &mut impl Iterator<Item = String>,
) -> Result<(), Box<dyn Error>> {
    let source_parent = arguments
        .next()
        .ok_or_else(|| invalid_argument("source parent directory path is required"))?;
    let source_device = parse_identity(
        arguments.next(),
        "source parent device identity is required",
    )?;
    let source_file = parse_identity(arguments.next(), "source parent file identity is required")?;
    let source = parse_name(arguments.next(), "source directory name is required")?;
    let source_entry_device =
        parse_identity(arguments.next(), "source entry device identity is required")?;
    let source_entry_file =
        parse_identity(arguments.next(), "source entry file identity is required")?;
    let destination_parent = arguments
        .next()
        .ok_or_else(|| invalid_argument("destination parent directory path is required"))?;
    let destination_device = parse_identity(
        arguments.next(),
        "destination parent device identity is required",
    )?;
    let destination_file = parse_identity(
        arguments.next(),
        "destination parent file identity is required",
    )?;
    let destination = parse_name(arguments.next(), "destination directory name is required")?;
    if arguments.next().is_some() {
        return Err(invalid_argument("unexpected filesystem authority argument").into());
    }

    let source_directory = open_verified_directory(&source_parent, (source_device, source_file))?;
    let destination_directory =
        open_verified_directory(&destination_parent, (destination_device, destination_file))?;
    rename_no_replace(
        &source_directory,
        &source,
        (source_entry_device, source_entry_file),
        &destination_directory,
        &destination,
    )?;
    Ok(())
}

fn rename_verified_entry_between_replace(
    arguments: &mut impl Iterator<Item = String>,
) -> Result<(), Box<dyn Error>> {
    let source_parent = arguments
        .next()
        .ok_or_else(|| invalid_argument("source parent directory path is required"))?;
    let source_device = parse_identity(
        arguments.next(),
        "source parent device identity is required",
    )?;
    let source_file = parse_identity(arguments.next(), "source parent file identity is required")?;
    let source = parse_name(arguments.next(), "source entry name is required")?;
    let source_entry_device =
        parse_identity(arguments.next(), "source entry device identity is required")?;
    let source_entry_file =
        parse_identity(arguments.next(), "source entry file identity is required")?;
    let destination_parent = arguments
        .next()
        .ok_or_else(|| invalid_argument("destination parent directory path is required"))?;
    let destination_device = parse_identity(
        arguments.next(),
        "destination parent device identity is required",
    )?;
    let destination_file = parse_identity(
        arguments.next(),
        "destination parent file identity is required",
    )?;
    let destination = parse_name(arguments.next(), "destination entry name is required")?;
    if arguments.next().is_some() {
        return Err(invalid_argument("unexpected filesystem authority argument").into());
    }

    let source_directory = open_verified_directory(&source_parent, (source_device, source_file))?;
    let destination_directory =
        open_verified_directory(&destination_parent, (destination_device, destination_file))?;
    rename_replace(
        &source_directory,
        &source,
        (source_entry_device, source_entry_file),
        &destination_directory,
        &destination,
    )?;
    Ok(())
}

/// Executes one versioned filesystem-authority protocol request from the
/// process argument vector.
///
/// # Errors
///
/// Returns the rejected operation and its underlying filesystem cause.
pub fn run() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args().skip(1);
    if arguments.next().as_deref() != Some("2") {
        return Err(invalid_argument("unsupported filesystem authority protocol version").into());
    }
    match arguments.next().as_deref() {
        Some("preflight") => {
            if arguments.next().is_some() {
                return Err(invalid_argument("unexpected filesystem authority argument").into());
            }
            Ok(())
        }
        Some("remove-directory") => remove_verified_directory(&mut arguments),
        Some("create-directory") => create_verified_directory(&mut arguments),
        Some("remove-directory-entry") => remove_verified_directory_entry(&mut arguments),
        Some("rename-directory-no-replace") => rename_verified_directory_no_replace(&mut arguments),
        Some("rename-directory-between-no-replace") => {
            rename_verified_directory_between_no_replace(&mut arguments)
        }
        Some("rename-entry-between-replace") => {
            rename_verified_entry_between_replace(&mut arguments)
        }
        Some("copy-file") => copy_verified_file(&mut arguments),
        Some("write-file") => write_verified_file(&mut arguments),
        _ => Err(invalid_argument("unsupported filesystem authority operation").into()),
    }
}

#[cfg(test)]
mod stable_file_tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use cap_std::ambient_authority;
    use cap_std::fs::Dir;

    use super::{
        confirm_stable_absolute_file, open_syncable_directory_ambient, path_is_proven_absent,
        read_stable_absolute_file, read_stable_file, read_stable_file_observed, sync_directory,
    };
    #[cfg(target_os = "linux")]
    use super::publish_bytes_no_replace;

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock must follow the Unix epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "hyper-fs-stable-file-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("temporary authority root must be unique");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn stable_read_is_exact_and_bounded() {
        let temporary = TemporaryDirectory::new();
        fs::write(temporary.path().join("state.json"), b"stable").unwrap();
        let directory = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();

        assert_eq!(
            read_stable_file(&directory, "state.json", 6).unwrap(),
            b"stable"
        );
        assert!(read_stable_file(&directory, "state.json", 5).is_err());
    }

    #[test]
    fn syncable_directory_accepts_a_metadata_barrier() {
        let temporary = TemporaryDirectory::new();
        let directory = open_syncable_directory_ambient(temporary.path()).unwrap();

        sync_directory(&directory).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unnamed_verified_publication_never_replaces_an_existing_destination() {
        let temporary = TemporaryDirectory::new();
        let directory = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();

        publish_bytes_no_replace(&directory, "mix.flac", b"verified").unwrap();
        assert_eq!(fs::read(temporary.path().join("mix.flac")).unwrap(), b"verified");

        assert!(publish_bytes_no_replace(&directory, "mix.flac", b"replacement").is_err());
        assert_eq!(fs::read(temporary.path().join("mix.flac")).unwrap(), b"verified");
    }

    #[test]
    fn stable_read_rejects_a_same_byte_rewrite() {
        let temporary = TemporaryDirectory::new();
        let path = temporary.path().join("state.json");
        fs::write(&path, b"stable").unwrap();
        let directory = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();

        let result = read_stable_file_observed(&directory, "state.json", 6, || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            fs::write(&path, b"stable")
        });

        assert!(result.is_err());
    }

    #[test]
    fn absolute_snapshot_rejects_content_change_and_entry_replacement() {
        let temporary = TemporaryDirectory::new();
        let path = temporary.path().join("source.wav");
        fs::write(&path, b"original").unwrap();
        let snapshot = read_stable_absolute_file(&path, 8).unwrap();
        let (_, evidence) = snapshot.into_parts();

        confirm_stable_absolute_file(&path, 8, &evidence).unwrap();

        fs::write(&path, b"mutated!").unwrap();
        assert!(confirm_stable_absolute_file(&path, 8, &evidence).is_err());

        fs::write(&path, b"original").unwrap();
        let replacement = temporary.path().join("replacement.wav");
        fs::write(&replacement, b"original").unwrap();
        fs::remove_file(&path).unwrap();
        fs::rename(&replacement, &path).unwrap();
        assert!(confirm_stable_absolute_file(&path, 8, &evidence).is_err());
    }

    #[test]
    fn retained_path_classification_distinguishes_present_and_missing() {
        let temporary = TemporaryDirectory::new();
        let parent = temporary.path().join("parent");
        let present = parent.join("project");
        fs::create_dir_all(&present).unwrap();

        assert!(!path_is_proven_absent(&present).unwrap());
        assert!(path_is_proven_absent(&parent.join("missing")).unwrap());
        assert!(path_is_proven_absent(&parent.join("missing/project")).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn retained_path_classification_does_not_call_a_dangling_link_absent() {
        use std::os::unix::fs::symlink;

        let temporary = TemporaryDirectory::new();
        let linked = temporary.path().join("linked");
        symlink(temporary.path().join("missing-target"), &linked).unwrap();

        assert!(path_is_proven_absent(&linked.join("project")).is_err());
        assert!(path_is_proven_absent(&linked).is_err());
    }
}

#[cfg(test)]
mod directory_entry_tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use cap_std::ambient_authority;
    use cap_std::fs::Dir;

    use super::{identity, remove_directory_entry};

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn new() -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "hyper-fs-authority-entry-{}-{nonce}",
                std::process::id(),
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn identity_bound_removal_preserves_a_replacement() {
        let temporary = TemporaryDirectory::new();
        let original = temporary.path().join("entry");
        let displaced = temporary.path().join("displaced");
        fs::create_dir(&original).unwrap();
        let parent = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
        let original_identity = identity(&parent.symlink_metadata("entry").unwrap());
        fs::rename(&original, &displaced).unwrap();
        fs::create_dir(&original).unwrap();
        fs::write(original.join("replacement"), b"preserve").unwrap();

        let result = remove_directory_entry(&parent, "entry", original_identity);

        assert!(result.is_err());
        assert_eq!(fs::read(original.join("replacement")).unwrap(), b"preserve");
        assert!(displaced.is_dir());
    }
}
