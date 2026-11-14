// 把自定义链接脚本与 4KB 页对齐参数传给 rust-lld，生成紧凑的静态 ET_EXEC
// （由内核 elf.rs 加载器加载）；并交叉编译 C PDU 编解码库（pdu.c）为静态库
// libpdu.a 链接进本二进制（freestanding，无 libc）。
use std::env;
use std::path::PathBuf;
use std::process::Command;

/// 返回候选命令中第一个存在于 PATH 且可执行的（都没找到则取末位，让
/// Command 报原始 NotFound）。
fn first_in_path(candidates: &[&str]) -> String {
    for name in candidates {
        if let Some(paths) = env::var_os("PATH") {
            for dir in env::split_paths(&paths) {
                let mut names = vec![name.to_string()];
                if cfg!(windows) {
                    for ext in env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT".into()).split(';') {
                        names.push(format!("{name}{ext}"));
                    }
                }
                for file in names {
                    let p = dir.join(file);
                    if !p.is_file() { continue; }
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        if !p.metadata().map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false) { continue; }
                    }
                    return p.to_string_lossy().into_owned();
                }
            }
        }
    }
    candidates.last().unwrap().to_string()
}

fn main() {
    let dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    println!("cargo:rustc-link-arg=-T{}/linker.ld", dir);
    // AArch64 lld 默认 LOAD 段按 64KB 对齐，导致 ELF 文件内大量空洞；
    // 压低到 4KB 使镜像紧凑（远小于内核 16KB 加载缓冲）。
    println!("cargo:rustc-link-arg=-z");
    println!("cargo:rustc-link-arg=max-page-size=0x1000");
    println!("cargo:rerun-if-changed=linker.ld");

    // ---- C PDU 编解码库 ----
    let target = env::var("TARGET").unwrap_or_default();
    let (cc, ar) = if target.contains("riscv") {
        // clang 交叉（宿主无 riscv gcc）：lld 链接，medium-any 代码模型。
        // ar 只做归档（架构无关）：优先 llvm-ar；发行版常只有带版本号的
        // llvm-ar-XX 而无通用链接（如 CI 的 ubuntu），回退 binutils ar。
        let ar = env::var("KUNXOS_AR").unwrap_or_else(|_| first_in_path(&["llvm-ar", "ar"]));
        (env::var("KUNXOS_CC").unwrap_or_else(|_| first_in_path(&["clang"])), ar)
    } else {
        (
            env::var("KUNXOS_CC").unwrap_or_else(|_| "aarch64-linux-gnu-gcc".to_string()),
            env::var("KUNXOS_AR").unwrap_or_else(|_| "aarch64-linux-gnu-ar".to_string()),
        )
    };
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    let obj = out.join("pdu.o");
    let lib = out.join("libpdu.a");

    let mut cc_cmd = Command::new(&cc);
    cc_cmd.args([
        "-nostdlib",
        "-ffreestanding",
        "-fno-stack-protector",
        "-fno-pic",
        "-fno-pie",
        "-O2",
        "-Wall",
        "-Wextra",
        "-c",
    ]);
    if target.contains("riscv") {
        cc_cmd.arg("--target=riscv64").arg("-march=rv64gc").arg("-mabi=lp64d").arg("-mcmodel=medany");
    }
    let status = cc_cmd
        .arg(format!("{}/src/pdu/pdu.c", dir))
        .arg("-o")
        .arg(&obj)
        .status()
        .unwrap_or_else(|e| panic!("failed to run C compiler {cc}: {e}; set KUNXOS_CC"));
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
    println!("cargo:rerun-if-env-changed=KUNXOS_CC");
    println!("cargo:rerun-if-env-changed=KUNXOS_AR");
    println!("cargo:rerun-if-env-changed=PATH");
    println!("cargo:rerun-if-env-changed=PATHEXT");
}
