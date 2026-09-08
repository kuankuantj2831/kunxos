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
    // 注意：不要对 ramdisk.img 发 rerun-if-changed —— 一旦构建脚本声明了
    // 任何 rerun-if-changed 路径，cargo 对 include_bytes! 依赖（dep-info）
    // 的变更检测会被该指纹路径遮蔽，实测 ramdisk 内容变化时跳过重编译，
    // objcopy 把陈旧 .rodata 重新包装成 kernel.img（测试标记间歇性丢失的
    // 根因）。标准 include_bytes! 依赖跟踪足以覆盖。
}
