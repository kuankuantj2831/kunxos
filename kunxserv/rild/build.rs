// 把自定义链接脚本与 4KB 页对齐参数传给 rust-lld，生成紧凑的静态 ET_EXEC
// （由内核 elf.rs 加载器加载）；并交叉编译 C PDU 编解码库（pdu.c）为静态库
// libpdu.a 链接进本二进制（freestanding，无 libc）。
use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    println!("cargo:rustc-link-arg=-T{}/linker.ld", dir);
    // AArch64 lld 默认 LOAD 段按 64KB 对齐，导致 ELF 文件内大量空洞；
    // 压低到 4KB 使镜像紧凑（远小于内核 16KB 加载缓冲）。
    println!("cargo:rustc-link-arg=-z");
    println!("cargo:rustc-link-arg=max-page-size=0x1000");
    println!("cargo:rerun-if-changed=linker.ld");

    // ---- C PDU 编解码库 ----
    let cc = env::var("KUNXOS_CC").unwrap_or_else(|_| "aarch64-linux-gnu-gcc".to_string());
    let ar = env::var("KUNXOS_AR").unwrap_or_else(|_| "aarch64-linux-gnu-ar".to_string());
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    let obj = out.join("pdu.o");
    let lib = out.join("libpdu.a");

    let status = Command::new(&cc)
        .args([
            "-nostdlib",
            "-ffreestanding",
            "-fno-stack-protector",
            "-fno-pic",
            "-fno-pie",
            "-O2",
            "-Wall",
            "-Wextra",
            "-c",
        ])
        .arg(format!("{}/src/pdu/pdu.c", dir))
        .arg("-o")
        .arg(&obj)
        .status()
        .expect("failed to run C compiler (aarch64-linux-gnu-gcc)");
    assert!(status.success(), "pdu.c compile failed");

    let status = Command::new(&ar)
        .arg("crs")
        .arg(&lib)
        .arg(&obj)
        .status()
        .expect("failed to run ar");
    assert!(status.success(), "ar failed");

    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=pdu");
    println!("cargo:rerun-if-changed=src/pdu/pdu.c");
    println!("cargo:rerun-if-changed=src/pdu/pdu.h");
}
