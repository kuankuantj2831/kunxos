// 与 networkd 相同：自定义链接脚本 + 4KB 页对齐，产出紧凑静态 ET_EXEC。
use std::env;

fn main() {
    let dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    println!("cargo:rustc-link-arg=-T{}/linker.ld", dir);
    println!("cargo:rustc-link-arg=-z");
    println!("cargo:rustc-link-arg=max-page-size=0x1000");
    println!("cargo:rerun-if-changed=linker.ld");
}