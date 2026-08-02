use version_compare::{compare_to, Cmp};

fn version_at_least(actual: &str, minimum: &str) -> bool {
    compare_to(actual, minimum, Cmp::Ge)
        .unwrap_or_else(|()| panic!("PipeWire version `{actual}` is not comparable"))
}

fn main() {
    println!("cargo:rustc-check-cfg=cfg(pipewire_1_2)");
    println!("cargo:rustc-check-cfg=cfg(pipewire_1_4)");

    let libraries = system_deps::Config::new()
        .probe()
        .expect("cannot find PipeWire 1.0 development files");
    let pipewire = libraries
        .get_by_name("libpipewire")
        .expect("PipeWire dependency probe did not return libpipewire");

    if version_at_least(&pipewire.version, "1.2") {
        println!("cargo:rustc-cfg=pipewire_1_2");
    }
    if version_at_least(&pipewire.version, "1.4") {
        println!("cargo:rustc-cfg=pipewire_1_4");
    }
}
