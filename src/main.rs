fn main() {
    if matches!(std::env::args().nth(1).as_deref(), Some("--version" | "-V")) {
        println!("image-convert {}", env!("CARGO_PKG_VERSION"));
    }
}
