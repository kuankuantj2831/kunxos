use std::env;

fn main() {
    // 获取 Cargo 清单目录，构造链接脚本的绝对路径，避免链接器工作目录不同导致找不到脚本
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    // 按 feature 选择链接脚本：QEMU（linker_qemu.ld）或 Raspberry Pi 4（linker_pi4.ld）
    let linker = if env::var("CARGO_FEATURE_PI4").is_ok() {
        "linker_pi4.ld"
    } else {
        "linker_qemu.ld"
    };
    // 将自定义链接脚本传给链接器（rustc 会转发给 rust-lld）
    println!("cargo:rustc-link-arg=-T{}/{}", manifest_dir, linker);
    // 链接脚本变化时触发重新构建
    println!("cargo:rerun-if-changed={}", linker);
    // 内嵌的 rootfs 镜像变化时触发重新构建（由顶层 make rootfs 生成）
    println!("cargo:rerun-if-changed=ramdisk.img");
}
