//! kosctl — KOS 卷宿主管理工具。
//!
//! ```text
//! kosctl mkfs   <img> <blocks>                 建盘
//! kosctl import <img> <dir> [prefix]           导入目录树（迁移入口）
//! kosctl export <img> <dir> [prefix]           导出目录树
//! kosctl ls     <img> <path>                   列目录
//! kosctl put    <img> <path> <file>            写入一个文件（不可变）
//! kosctl putm   <img> <path> <file>            写入可变记录
//! kosctl append <img> <path> <file>            追加写（可变记录）
//! kosctl get    <img> <path> [file]            读出一个对象
//! kosctl unlink <img> <path>                   删除条目
//! kosctl verify <img>                          全树严格验证（重建 Merkle）
//! kosctl gc     <img>                          回收不可达孤儿对象
//! kosctl stat   <img>                          卷状态
//! ```

use kos::block::file::FileBlockDevice;
use kos::format::object::ObjectType;
use kos::store::{perm, KosStore, VerifyLevel};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match run(&args) {
        Ok(msg) => {
            if !msg.is_empty() {
                println!("{msg}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("kosctl: {e}");
            ExitCode::FAILURE
        }
    }
}

fn open_rw(img: &str, create: Option<u64>) -> std::io::Result<FileBlockDevice> {
    FileBlockDevice::open(Path::new(img), create)
}

fn run(args: &[String]) -> Result<String, Box<dyn std::error::Error>> {
    let cmd = args.get(1).map(String::as_str).unwrap_or("");
    match cmd {
        "mkfs" => {
            let img = arg(args, 2)?;
            let blocks: u64 = arg(args, 3)?.parse()?;
            let dev = open_rw(img, Some(blocks))?;
            let s = KosStore::mkfs(dev)?;
            Ok(format!(
                "已建盘：{} 块，generation {}，空闲 {}",
                blocks,
                s.generation(),
                s.free_blocks()
            ))
        }
        "import" => {
            let img = arg(args, 2)?;
            let src = PathBuf::from(arg(args, 3)?);
            let prefix = args.get(4).cloned().unwrap_or_default();
            let mut s = KosStore::open(open_rw(img, None)?, VerifyLevel::HashOnly)?;
            let n = import_tree(&mut s, &src, &prefix)?;
            Ok(format!("已导入 {n} 个条目，generation {}", s.generation()))
        }
        "export" => {
            let img = arg(args, 2)?;
            let dst = PathBuf::from(arg(args, 3)?);
            let prefix = args.get(4).cloned().unwrap_or_default();
            let mut s = KosStore::open(open_rw(img, None)?, VerifyLevel::Full)?;
            let n = export_tree(&mut s, &dst, &prefix)?;
            Ok(format!("已导出 {n} 个条目"))
        }
        "ls" => {
            let img = arg(args, 2)?;
            let path = args.get(3).cloned().unwrap_or_else(|| "/".into());
            let mut s = KosStore::open(open_rw(img, None)?, VerifyLevel::HashOnly)?;
            let mut out = String::new();
            for e in s.list(&path)? {
                let t = match e.entry_type {
                    ObjectType::Dir => "d",
                    ObjectType::Symlink => "l",
                    _ => "-",
                };
                out.push_str(&format!(
                    "{t} {:>6} {:>4} {:08x} {}\n",
                    e.owner_uid, e.capability, 0, e.name
                ));
            }
            Ok(out.trim_end().to_string())
        }
        "putm" => {
            let img = arg(args, 2)?;
            let path = arg(args, 3)?;
            let file = arg(args, 4)?;
            let data = std::fs::read(&file)?;
            let mut s = KosStore::open(open_rw(img, None)?, VerifyLevel::HashOnly)?;
            s.write_mutable(path, &data, 0, perm::DEFAULT)?;
            Ok(format!("已写入可变记录 {path}（{} 字节）", data.len()))
        }
        "append" => {
            let img = arg(args, 2)?;
            let path = arg(args, 3)?;
            let file = arg(args, 4)?;
            let data = std::fs::read(&file)?;
            let mut s = KosStore::open(open_rw(img, None)?, VerifyLevel::HashOnly)?;
            let len = s.append(path, &data, 0)?;
            Ok(format!("已追加 {} 字节，{path} 现长 {len} 字节", data.len()))
        }
        "put" => {
            let img = arg(args, 2)?;
            let path = arg(args, 3)?;
            let file = arg(args, 4)?;
            let data = std::fs::read(&file)?;
            let mut s = KosStore::open(open_rw(img, None)?, VerifyLevel::HashOnly)?;
            s.write(path, &data, 0, perm::DEFAULT)?;
            Ok(format!("已写入 {path}（{} 字节）", data.len()))
        }
        "get" => {
            let img = arg(args, 2)?;
            let path = arg(args, 3)?;
            let mut s = KosStore::open(open_rw(img, None)?, VerifyLevel::Full)?;
            let data = s.open_path(path, 0)?;
            match args.get(4) {
                Some(f) => {
                    std::fs::write(f, &data)?;
                    Ok(format!("已读出 {} 字节 → {f}", data.len()))
                }
                None => Ok(format!("{} 字节（-- 用第 4 参数指定输出文件）", data.len())),
            }
        }
        "unlink" => {
            let img = arg(args, 2)?;
            let path = arg(args, 3)?;
            let mut s = KosStore::open(open_rw(img, None)?, VerifyLevel::HashOnly)?;
            s.unlink(path, 0)?;
            Ok(format!("已删除 {path}"))
        }
        "verify" => {
            let img = arg(args, 2)?;
            let mut s = KosStore::open(open_rw(img, None)?, VerifyLevel::Full)?;
            let (files, bytes) = walk_verify(&mut s, "")?;
            Ok(format!(
                "验证通过：{files} 个对象共 {bytes} 字节，Merkle 全量比对无一失配"
            ))
        }
        "gc" => {
            let img = arg(args, 2)?;
            let mut s = KosStore::open(open_rw(img, None)?, VerifyLevel::HashOnly)?;
            let (dead, freed) = s.gc()?;
            Ok(format!(
                "GC 完成：清扫 {dead} 个孤儿对象，回收 {freed} 块（{} KB）",
                freed * 4
            ))
        }
        "stat" => {
            let img = arg(args, 2)?;
            let s = KosStore::open(open_rw(img, None)?, VerifyLevel::HashOnly)?;
            Ok(format!(
                "generation={} objects={} free_blocks={}/{}",
                s.generation(),
                s.object_count(),
                s.free_blocks(),
                s.pool_blocks()
            ))
        }
        _ => Err("用法：kosctl <mkfs|import|export|ls|put|get|unlink|verify|stat> …".into()),
    }
}

fn arg(args: &[String], i: usize) -> Result<&str, Box<dyn std::error::Error>> {
    args.get(i)
        .map(String::as_str)
        .ok_or_else(|| format!("缺少第 {i} 个参数").into())
}

fn import_tree(
    s: &mut KosStore<FileBlockDevice>,
    src: &Path,
    prefix: &str,
) -> Result<usize, Box<dyn std::error::Error>> {
    // 建目录（若非根）。
    if !prefix.is_empty() {
        mkdir_p(s, prefix)?;
    }
    let mut n = 0usize;
    let mut entries: Vec<_> = std::fs::read_dir(src)?
        .filter_map(|e| e.ok())
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let path = format!("{prefix}/{name}");
        let ft = e.file_type()?;
        if ft.is_dir() {
            s.mkdir(&path, 0, perm::DIR_DEFAULT)?;
            n += 1 + import_tree(s, &e.path(), &path)?;
        } else if ft.is_symlink() {
            let target = std::fs::read_link(e.path())?;
            s.symlink(&path, target.as_os_str().as_encoded_bytes(), 0)?;
            n += 1;
        } else {
            let mode = e.metadata()?.permissions();
            #[cfg(unix)]
            let caps = {
                use std::os::unix::fs::PermissionsExt;
                let m = mode.mode();
                // Unix 9 位权限 → KOS 权限位（丢弃 setuid 等高位）。
                (m & 0o777) as u32
            };
            #[cfg(not(unix))]
            let caps = perm::DEFAULT;
            let data = std::fs::read(e.path())?;
            s.write(&path, &data, 0, caps)?;
            n += 1;
        }
    }
    Ok(n)
}

fn mkdir_p(s: &mut KosStore<FileBlockDevice>, path: &str) -> Result<(), kos::KosError> {
    let mut cur = String::new();
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        cur = format!("{cur}/{seg}");
        if s.list(&cur).is_err() {
            s.mkdir(&cur, 0, perm::DIR_DEFAULT)?;
        }
    }
    Ok(())
}

fn export_tree(
    s: &mut KosStore<FileBlockDevice>,
    dst: &Path,
    prefix: &str,
) -> Result<usize, Box<dyn std::error::Error>> {
    std::fs::create_dir_all(dst)?;
    let mut n = 0usize;
    for e in s.list(prefix)? {
        let name = e.name.clone();
        let target = dst.join(&name);
        match e.entry_type {
            ObjectType::Dir => {
                std::fs::create_dir_all(&target)?;
                let sub = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}/{name}")
                };
                n += 1 + export_tree(s, &target, &sub)?;
            }
            ObjectType::Symlink => {
                let data = s.readlink(&format!("{prefix}/{name}"))?;
                #[cfg(unix)]
                {
                    use std::os::unix::ffi::OsStringExt;
                    std::os::unix::fs::symlink(
                        std::ffi::OsString::from_vec(data.clone().into_iter().collect()),
                        &target,
                    )
                    .or_else(|_| std::fs::write(&target, &data))?; // 目标已存在则降级为常规文件
                }
                #[cfg(not(unix))]
                std::fs::write(&target, &data)?;
                n += 1;
            }
            _ => {
                let data = s.open_path(&format!("{prefix}/{name}"), 0)?;
                std::fs::write(&target, &data)?;
                n += 1;
            }
        }
    }
    Ok(n)
}

fn walk_verify(
    s: &mut KosStore<FileBlockDevice>,
    prefix: &str,
) -> Result<(usize, u64), Box<dyn std::error::Error>> {
    let mut files = 0usize;
    let mut bytes = 0u64;
    for e in s.list(prefix)? {
        let sub = if prefix.is_empty() {
            e.name.clone()
        } else {
            format!("{prefix}/{}", e.name)
        };
        if e.entry_type == ObjectType::Dir {
            let (f, b) = walk_verify(s, &sub)?;
            files += f;
            bytes += b;
        } else {
            let data = s.open_path(&sub, 0)?; // Full 模式：逐块 Merkle 比对
            files += 1;
            bytes += data.len() as u64;
        }
    }
    Ok((files, bytes))
}
