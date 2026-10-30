// Compiles libwebp's encoder (vendor/libwebp, a submodule at tag v1.6.0) for src/webp.rs.
fn main() {
    let root = std::path::Path::new("vendor/libwebp");
    let mut build = cc::Build::new();
    for dir in ["src/enc", "src/dsp", "src/utils", "sharpyuv"] {
        println!("cargo:rerun-if-changed=vendor/libwebp/{dir}");
        let files = std::fs::read_dir(root.join(dir))
            .expect("vendor/libwebp is missing: run git submodule update --init");
        for file in files {
            let path = file.unwrap().path();
            if path.extension().is_some_and(|e| e == "c") {
                build.file(path);
            }
        }
    }
    build
        .include(root)
        .define("NDEBUG", None)
        .warnings(false)
        .compile("webp");
}
