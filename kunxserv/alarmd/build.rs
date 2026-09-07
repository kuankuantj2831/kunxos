// 把自定义链接脚本与 4KB 页对齐参数传给 rust-lld，
// 生成紧凑的静态 ET_EXEC（由内核 elf.rs 加载器加载）。
use std::env;

fn main() {
    let dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    println!("cargo:rustc-link-arg=-T{}/linker.ld", dir);
    println!("cargo:rustc-link-arg=-z");
    println!("cargo:rustc-link-arg=max-page-size=0x1000");
    println!("cargo:rerun-if-changed=linker.ld");
}