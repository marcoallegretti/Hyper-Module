use std::error::Error;
use std::fs::File;
use std::io;
use std::path::Path;

use hyper_audio_source::{AudioContainer, DecodeLimits, decode_file, decode_open_file};

#[cfg(unix)]
use std::os::fd::AsFd;
#[cfg(windows)]
use std::os::windows::io::AsHandle;

fn invalid_argument(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn parse_argument<T: std::str::FromStr>(
    value: Option<String>,
    message: &'static str,
) -> Result<T, Box<dyn Error>>
where
    T::Err: Error + 'static,
{
    value
        .ok_or_else(|| invalid_argument(message))?
        .parse::<T>()
        .map_err(Into::into)
}

fn parse_container(value: &str) -> Result<AudioContainer, io::Error> {
    match value {
        "wav" => Ok(AudioContainer::Wav),
        "flac" => Ok(AudioContainer::Flac),
        "mp3" => Ok(AudioContainer::Mp3),
        "aiff" | "aif" => Ok(AudioContainer::Aiff),
        _ => Err(invalid_argument(
            "audio container must be wav, flac, mp3, or aiff",
        )),
    }
}

#[cfg(unix)]
fn duplicate_standard_input() -> io::Result<File> {
    Ok(File::from(io::stdin().as_fd().try_clone_to_owned()?))
}

#[cfg(windows)]
fn duplicate_standard_input() -> io::Result<File> {
    Ok(File::from(io::stdin().as_handle().try_clone_to_owned()?))
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args().skip(1);
    if arguments.next().as_deref() != Some("1") {
        return Err(invalid_argument("unsupported audio validator protocol version").into());
    }
    let path = arguments
        .next()
        .ok_or_else(|| invalid_argument("audio path is required"))?;
    let container = parse_container(
        &arguments
            .next()
            .ok_or_else(|| invalid_argument("audio container is required"))?,
    )?;
    let sample_rate_hz = parse_argument(arguments.next(), "sample rate is required")?;
    let maximum_file_bytes = parse_argument(arguments.next(), "file byte limit is required")?;
    let expected_frames: u64 = parse_argument(arguments.next(), "frame limit is required")?;
    let maximum_decoded_sample_bytes =
        parse_argument(arguments.next(), "decoded byte limit is required")?;
    if arguments.next().is_some() {
        return Err(invalid_argument("unexpected audio validator argument").into());
    }

    let limits = DecodeLimits::new(
        maximum_file_bytes,
        expected_frames,
        maximum_decoded_sample_bytes,
    )?;
    let decoded = if path == "-" {
        decode_open_file(
            duplicate_standard_input()?,
            container,
            sample_rate_hz,
            limits,
        )?
    } else {
        decode_file(Path::new(&path), container, sample_rate_hz, limits)?
    };
    if u64::try_from(decoded.frame_count())? != expected_frames {
        return Err(invalid_argument("decoded audio frame count does not match metadata").into());
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("audio validation failed: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::{AudioContainer, parse_container};

    #[test]
    fn parses_every_supported_audio_container() {
        assert_eq!(parse_container("wav").unwrap(), AudioContainer::Wav);
        assert_eq!(parse_container("flac").unwrap(), AudioContainer::Flac);
        assert_eq!(parse_container("mp3").unwrap(), AudioContainer::Mp3);
        assert_eq!(parse_container("aiff").unwrap(), AudioContainer::Aiff);
        assert_eq!(parse_container("aif").unwrap(), AudioContainer::Aiff);
        assert!(parse_container("ogg").is_err());
    }
}
