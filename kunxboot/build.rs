use std::env;

fn main() {
    // 按 feature 选择链接脚本：QEMU（linker.ld，加载于 0x40200000）
    // 或 Raspberry Pi 4（linker_pi4.ld，加载于 0x80000）。
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let linker = if env::var("CARGO_FEATURE_PI4").is_ok() {
        "linker_pi4.ld"
    } else {
        "linker.ld"
    };
    println!("cargo:rustc-link-arg=-T{}/{}", manifest_dir, linker);
    println!("cargo:rerun-if-changed={}", linker);
}
