use std::error::Error;
use std::io;
use std::path::{Component, Path};

use cap_fs_ext::MetadataExt;
use cap_std::ambient_authority;
use cap_std::fs::{Dir, Metadata};

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
    destination_directory: &Dir,
    destination: &str,
) -> io::Result<()> {
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
    destination_directory: &Dir,
    destination: &str,
) -> io::Result<()> {
    match destination_directory.symlink_metadata(destination) {
        Ok(_) => return Err(io::ErrorKind::AlreadyExists.into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    match source_directory.rename(source, destination_directory, destination) {
        Ok(()) => Ok(()),
        Err(error) if destination_directory.symlink_metadata(destination).is_ok() => {
            Err(io::ErrorKind::AlreadyExists.into())
        }
        Err(error) => Err(error),
    }
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
    let destination = parse_name(arguments.next(), "destination directory name is required")?;
    if arguments.next().is_some() {
        return Err(invalid_argument("unexpected filesystem authority argument").into());
    }

    let directory = open_verified_directory(&parent, (expected_device, expected_file))?;
    rename_no_replace(&directory, &source, &directory, &destination)?;
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
    source_directory.rename(source, &destination_directory, destination)?;
    Ok(())
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args().skip(1);
    if arguments.next().as_deref() != Some("1") {
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
