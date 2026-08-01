use std::error::Error;
use std::io;
use std::path::{Component, Path};

use cap_fs_ext::MetadataExt;
use cap_std::ambient_authority;
use cap_std::fs::{Dir, Metadata};

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_rename {
    use std::ffi::OsStr;
    use std::io;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use std::ptr::{addr_of_mut, copy_nonoverlapping, null_mut};

    use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
    use cap_std::fs::{Dir, File, OpenOptions, OpenOptionsExt};
    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS};
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileRenameInfo,
        GetFinalPathNameByHandleW, SetFileInformationByHandle,
    };

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
        let mut destination_name = final_directory_path(destination_directory)?;
        if !destination_name.ends_with(&['\\' as u16]) {
            destination_name.push('\\' as u16);
        }
        destination_name.extend(OsStr::new(destination).encode_wide());
        let name_bytes = destination_name
            .len()
            .checked_mul(size_of::<u16>())
            .and_then(|length| u32::try_from(length).ok())
            .ok_or_else(|| invalid_argument("destination entry name is too long"))?;
        destination_name.push(0);
        let buffer_bytes = size_of::<FILE_RENAME_INFO>()
            .checked_add(name_bytes as usize)
            .ok_or_else(|| invalid_argument("rename information size overflow"))?;
        let storage_words = buffer_bytes.div_ceil(size_of::<usize>());
        let mut storage = vec![0_usize; storage_words];
        let information = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();

        // SAFETY: `storage` is aligned to `usize`, sized for the fixed header and
        // complete UTF-16 name, and remains alive for the duration of the call.
        // Both handles are owned Rust values. The source was opened with DELETE
        // access, and the destination name is one validated path component.
        let result = unsafe {
            (*information).Anonymous.ReplaceIfExists = replace;
            (*information).RootDirectory = null_mut();
            (*information).FileNameLength = name_bytes;
            copy_nonoverlapping(
                destination_name.as_ptr(),
                addr_of_mut!((*information).FileName).cast::<u16>(),
                destination_name.len(),
            );
            SetFileInformationByHandle(
                source_handle.as_raw_handle(),
                FileRenameInfo,
                information.cast(),
                u32::try_from(buffer_bytes)
                    .map_err(|_| invalid_argument("rename information is too large"))?,
            )
        };
        if result != 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if matches!(
            error.raw_os_error().map(i32::cast_unsigned),
            Some(ERROR_ALREADY_EXISTS | ERROR_FILE_EXISTS)
        ) {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        Err(error)
    }

    fn final_directory_path(directory: &Dir) -> io::Result<Vec<u16>> {
        // SAFETY: The first call supplies no buffer and requests the required
        // length. The second call owns a buffer of that reported size and the
        // directory handle remains valid throughout both calls.
        let required =
            unsafe { GetFinalPathNameByHandleW(directory.as_raw_handle(), null_mut(), 0, 0) };
        if required == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut path = vec![0_u16; required as usize];
        // SAFETY: `path` is writable for its full reported capacity and the
        // directory handle remains valid for the duration of the call.
        let written = unsafe {
            GetFinalPathNameByHandleW(
                directory.as_raw_handle(),
                path.as_mut_ptr(),
                u32::try_from(path.len())
                    .map_err(|_| invalid_argument("directory path is too long"))?,
                0,
            )
        };
        if written == 0 {
            return Err(io::Error::last_os_error());
        }
        if written as usize >= path.len() {
            return Err(invalid_argument(
                "directory path changed while being resolved",
            ));
        }
        path.truncate(written as usize);
        Ok(path)
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
}

fn invalid_argument(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn identity(metadata: &Metadata) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
}

fn parse_identity(value: Option<String>, message: &'static str) -> Result<u64, Box<dyn Error>> {
    Ok(value
        .ok_or_else(|| invalid_argument(message))?
        .parse::<u64>()?)
}

fn parse_name(value: Option<String>, message: &'static str) -> Result<String, Box<dyn Error>> {
    let value = value.ok_or_else(|| invalid_argument(message))?;
    let mut components = Path::new(&value).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(invalid_argument("filesystem entry name must be one normal component").into());
    }
    Ok(value)
}

fn open_verified_directory(
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
fn rename_no_replace(
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
fn rename_no_replace(
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
fn rename_replace(
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
fn rename_replace(
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

fn run() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args().skip(1);
    if arguments.next().as_deref() != Some("2") {
        return Err(invalid_argument("unsupported filesystem authority protocol version").into());
    }
    match arguments.next().as_deref() {
        Some("remove-directory") => remove_verified_directory(&mut arguments),
        Some("rename-directory-no-replace") => rename_verified_directory_no_replace(&mut arguments),
        Some("rename-directory-between-no-replace") => {
            rename_verified_directory_between_no_replace(&mut arguments)
        }
        Some("rename-entry-between-replace") => {
            rename_verified_entry_between_replace(&mut arguments)
        }
        _ => Err(invalid_argument("unsupported filesystem authority operation").into()),
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("filesystem authority operation failed: {error}");
        let destination_exists = error
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == io::ErrorKind::AlreadyExists);
        std::process::exit(if destination_exists { 2 } else { 1 });
    }
}
