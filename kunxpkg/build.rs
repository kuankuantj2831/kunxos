// 把自定义链接脚本与页对齐参数传给 rust-lld，
// 生成紧凑的静态 ET_EXEC（由内核 elf.rs 加载器加载）。
//
// 说明：`max-page-size=0x100` 把段在文件内的偏移对齐降到 256 字节，
// 从而压缩 ELF 文件体积（满足「二进制 < 16KB」）。虚拟地址仍由 linker.ld
// 的 `ALIGN(0x1000)` 保持 4KB 页对齐——内核加载器按页映射并逐字节拷贝
// 段内容（见 elf.rs 的 `load`），只要求段数据在文件内连续，不要求
// p_offset 与 p_vaddr 按 4KB 同余，因此文件内更小的对齐是安全的。
use std::env;

fn main() {
    let dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    println!("cargo:rustc-link-arg=-T{}/linker.ld", dir);
    println!("cargo:rustc-link-arg=-z");
    println!("cargo:rustc-link-arg=max-page-size=0x100");
    println!("cargo:rerun-if-changed=linker.ld");
}
