use hyper_recording_journal::{PublicationOutcome, publish_wav};
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::path::PathBuf;

const USAGE: &str = "Usage: hyper-recording-recover <JOURNAL> <OUTPUT_WAV>\n\
Recover committed audio from a stopped recording journal into a stereo float WAV.\n\
An incomplete trailing chunk may be removed from the journal. The journal is retained.\n\
Existing, different output media is never replaced. Capture continuity is not verified.\n";

fn run(arguments: impl IntoIterator<Item = OsString>) -> Result<(), String> {
    let mut arguments = arguments.into_iter();
    let _executable = arguments.next();
    let Some(first) = arguments.next() else {
        return Err(USAGE.to_owned());
    };
    if first == OsStr::new("--help") || first == OsStr::new("-h") {
        if arguments.next().is_some() {
            return Err(USAGE.to_owned());
        }
        print!("{USAGE}");
        return Ok(());
    }
    if first == OsStr::new("--version") {
        if arguments.next().is_some() {
            return Err(USAGE.to_owned());
        }
        println!("hyper-recording-recover {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let journal = PathBuf::from(first);
    let Some(output) = arguments.next().map(PathBuf::from) else {
        return Err(USAGE.to_owned());
    };
    if arguments.next().is_some()
        || !output
            .extension()
            .and_then(OsStr::to_str)
            .is_some_and(|extension| extension.eq_ignore_ascii_case("wav"))
    {
        return Err(USAGE.to_owned());
    }
    let published = publish_wav(&journal, &output).map_err(|error| error.to_string())?;
    let action = match published.outcome() {
        PublicationOutcome::Published => "Published",
        PublicationOutcome::AlreadyPublished => "Already published",
    };
    let mut hash = String::with_capacity(64);
    for byte in published.wav_sha256() {
        write!(hash, "{byte:02x}").expect("writing to a String cannot fail");
    }
    println!(
        "{action} {}: {} frames, {} Hz, {} channels; SHA-256 {}",
        published.final_wav_path().display(),
        published.frames(),
        published.sample_rate_hz(),
        published.channels(),
        hash
    );
    println!("Journal retained: {}", published.journal_path().display());
    Ok(())
}

fn main() {
    if let Err(error) = run(std::env::args_os()) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::run;
    use std::ffi::OsString;

    #[test]
    fn requires_exactly_two_paths_and_wav_output() {
        for arguments in [
            vec!["recover"],
            vec!["recover", "take.journal"],
            vec!["recover", "take.journal", "take.mp3"],
            vec!["recover", "take.journal", "take.wav", "extra"],
        ] {
            assert!(run(arguments.into_iter().map(OsString::from)).is_err());
        }
    }

    #[test]
    fn inspection_commands_accept_no_extra_arguments() {
        for flag in ["--help", "--version"] {
            assert!(run(["recover", flag].into_iter().map(OsString::from)).is_ok());
            assert!(run(["recover", flag, "extra"].into_iter().map(OsString::from)).is_err());
        }
    }
}
