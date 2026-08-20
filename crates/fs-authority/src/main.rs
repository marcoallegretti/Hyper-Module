use std::io;

fn main() {
    if let Err(error) = hyper_fs_authority::run() {
        eprintln!("filesystem authority operation failed: {error}");
        let destination_exists = error
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == io::ErrorKind::AlreadyExists);
        std::process::exit(if destination_exists { 2 } else { 1 });
    }
}
