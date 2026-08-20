fn main() {
    std::thread::Builder::new()
        .name("slint-compiler".to_owned())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| slint_build::compile("ui/main.slint").expect("desktop UI must compile"))
        .expect("Slint compiler thread must start")
        .join()
        .expect("Slint compiler thread must complete");
}
