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
    // 内嵌 rootfs 镜像变化时触发重新构建（由顶层 make rootfs / test-* 生成）。
    //
    // ★ 必须显式跟踪：本 build.rs 声明了 linker 的 rerun-if-changed 之后，
    // cargo 对 include_bytes! 依赖（dep-info）的变更检测被该指纹遮蔽 ——
    // cb9ca35 曾试图删除 ramdisk 一行来恢复 dep-info 跟踪，但 linker 行
    // 本身仍在遮蔽：纯 rootfs 变化（sfsrace/audio 测试标记、rootfs 内容
    // 更新）不触发重编译，objcopy 把陈旧 .rodata 重新包装成 kernel.img
    // （表象：测试标记"间歇性"丢失，sfsrace/audiotest 一直 FAIL）。
    // 本包唯一的 include_bytes 就是 ramdisk.img，显式跟踪即完备。
    println!("cargo:rerun-if-changed=ramdisk.img");
}
