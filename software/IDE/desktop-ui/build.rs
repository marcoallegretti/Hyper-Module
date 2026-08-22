fn main() {
    let debug_info = std::env::var("PROFILE").is_ok_and(|profile| profile != "release");
    std::thread::Builder::new()
        .name("slint-compiler".to_owned())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let config = slint_build::CompilerConfiguration::new().with_debug_info(debug_info);
            slint_build::compile_with_config("ui/main.slint", config)
                .expect("desktop UI must compile");
        })
        .expect("Slint compiler thread must start")
        .join()
        .expect("Slint compiler thread must complete");
}
