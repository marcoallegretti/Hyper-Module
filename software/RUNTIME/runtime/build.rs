fn main() {
    let debug_info = std::env::var("PROFILE").is_ok_and(|profile| profile != "release");
    let config = slint_build::CompilerConfiguration::new().with_debug_info(debug_info);
    slint_build::compile_with_config("ui/operator.slint", config)
        .expect("Runtime operator UI must compile");
}
