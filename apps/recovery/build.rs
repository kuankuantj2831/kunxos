/* 确保链接产物为独立 ET_EXEC（与其它用户态 ELF 一致）。 */

use std::env;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    println!("cargo:rustc-link-arg=-T{}/linker.ld", manifest_dir);
    println!("cargo:rerun-if-changed=linker.ld");
}