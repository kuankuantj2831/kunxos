//! kosctl — KOS 卷宿主管理工具。
//!
//! ```text
//! kosctl mkfs   <img> <blocks>                 建盘
//! kosctl mkdir  <img> <path>                   建目录
//! kosctl import <img> <dir> [prefix]           导入目录树（迁移入口）
//! kosctl export <img> <dir> [prefix]           导出目录树
//! kosctl ls     <img> <path>                   列目录
//! kosctl put    <img> <path> <file>            写入一个文件（不可变）
//! kosctl putm   <img> <path> <file>            写入可变记录
//! kosctl append <img> <path> <file>            追加写（可变记录）
//! kosctl get    <img> <path> [file]            读出一个对象
//! kosctl unlink <img> <path>                   删除条目
//! kosctl verify <img>                          全树严格验证（重建 Merkle）
//! kosctl check  <img>                          fsck：全树 Full 验证 + 孤儿报告
//! kosctl keygen                               生成 Ed25519 种子（hex）
//! kosctl sign   <img> <seed-hex>               签名活动 manifest（打印公钥）
//! kosctl bench  <img>                           性能护栏（append/write_range/mount/GC 计时）
//! kosctl gc     <img>                          回收不可达孤儿对象
//! kosctl trunc  <img> <path> <len>             截断/扩展（可变记录）
//! kosctl upbegin <img> <path> <len>            开启断点续传会话
//! kosctl uppush  <img> <path> <file> [off]     推送数据（断点续传）
//! kosctl upstat  <img> <path>                  查看续传进度
//! kosctl upfin   <img> <path>                  完成并原子发布
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
        "keygen" => {
            #[cfg(feature = "sign")]
            {
                let mut seed = [0u8; 32];
                use std::io::Read;
                std::fs::File::open("/dev/urandom")?.read_exact(&mut seed)?;
                Ok(format!("seed: {}", hex_str(&seed)))
            }
            #[cfg(not(feature = "sign"))]
            Err("kosctl 需要 sign feature".into())
        }
        "sign" => {
            #[cfg(feature = "sign")]
            {
                let img = arg(args, 2)?;
                let seed = parse_hex32(arg(args, 3)?)?;
                let mut s = KosStore::open(open_rw(img, None)?, VerifyLevel::HashOnly)?;
                let pk = kunxsec::ed25519::public_key(&seed);
                let payload = s.manifest_payload();
                let sig = kunxsec::ed25519::sign(&seed, &pk, &payload);
                s.resign(pk, sig)?;
                Ok(format!("manifest 已签名\ntrust-pk: {}", hex_str(&pk)))
            }
            #[cfg(not(feature = "sign"))]
            Err("kosctl 需要 sign feature".into())
        }
        "mkdir" => {
            let img = arg(args, 2)?;
            let path = arg(args, 3)?;
            let mut s = KosStore::open(open_rw(img, None)?, VerifyLevel::HashOnly)?;
            s.mkdir(path, 0, 0o755)?;
            Ok(format!("已创建目录 {path}"))
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
        "bench" => {
            let img = arg(args, 2)?;
            // 建新卷跑基准，不动用户数据。
            let img = format!("{img}.bench.tmp");
            let dev = FileBlockDevice::open(Path::new(&img), Some(8192))?;
            let mut s = KosStore::mkfs(dev)?;
            s.set_flush_policy(kos::store::FlushPolicy::Batched(32));
            let t = std::time::Instant::now();
            s.write_mutable("/bench", &[0u8; 4096], 0, perm::DEFAULT)?;
            let create = t.elapsed();
            let t = std::time::Instant::now();
            for i in 0..200 {
                let line = format!("bench-line-{i:04}\n");
                s.append("/bench", line.as_bytes(), 0)?;
            }
            let appends = t.elapsed();
            let per_append = appends.as_micros() as f64 / 200.0;
            let t = std::time::Instant::now();
            for i in 0..100 {
                s.write_range("/bench", 4096 + i * 100, b"PATCHPAD!", 0)?;
            }
            let ranges = t.elapsed();
            let per_range = ranges.as_micros() as f64 / 100.0;
            let t = std::time::Instant::now();
            let _ = s.open_path("/bench", 0)?;
            let read1 = t.elapsed();
            s.sync_now().ok();
            let d = s.take_device();
            let t = std::time::Instant::now();
            let mut s2 = KosStore::open(d, VerifyLevel::HashOnly)?;
            let mount = t.elapsed();
            let fast = s2.was_fast_mounted();
            let t = std::time::Instant::now();
            s2.set_flush_policy(kos::store::FlushPolicy::Batched(32));
            let (dead, freed) = s2.gc()?;
            let gc = t.elapsed();
            std::fs::remove_file(&img)?;
            Ok(format!(
                "bench（8192 块卷）：建记录 {:?}；append {} 次 {}µs（{per_append:.0}µs/次）；\nwrite_range 100 次 {}µs（{per_range:.0}µs/次）；整读 {:?}；挂载 {:?}（快速路径={fast}）；GC {dead} 对象 {freed} 块 {:?}",
                create, 200, appends.as_micros(), ranges.as_micros(), read1, mount, gc
            ))
        }
        "check" => {
            let img = arg(args, 2)?;
            let mut s = KosStore::open(open_rw(img, None)?, VerifyLevel::Full)?;
            // 全树 Full 验证（复用 walk_verify；任一对象损坏即报错）。
            let (files, bytes) = walk_verify(&mut s, "")?;
            // 孤儿统计：可达闭包之外即孤儿。
            let mut reachable = std::collections::HashSet::new();
            reachable.insert(s.root_hash()); // 根目录对象本身可达
            fn walk(s: &mut KosStore<FileBlockDevice>, path: &str, set: &mut std::collections::HashSet<[u8; 32]>) {
                if let Ok(entries) = s.list(path) {
                    for e in entries {
                        set.insert(e.content_hash);
                        if e.entry_type == ObjectType::Dir {
                            let sub = if path.is_empty() {
                                e.name.clone()
                            } else {
                                format!("{path}/{}", e.name)
                            };
                            walk(s, &sub, set);
                        }
                    }
                }
            }
            walk(&mut s, "", &mut reachable);
            let orphans = s.object_count() - reachable.len();
            let (dead, freed) = s.gc()?;
            Ok(format!(
                "卷健康：{files} 个对象 {bytes} 字节 Full 验证通过；发现孤儿 {orphans} 个，已回收 {dead} 个（{} 块）",
                freed
            ))
        }
        "upbegin" => {
            let img = arg(args, 2)?;
            let path = arg(args, 3)?;
            let len: u64 = arg(args, 4)?.parse()?;
            let mut s = KosStore::open(open_rw(img, None)?, VerifyLevel::HashOnly)?;
            s.upload_begin(path, len, 0, perm::DEFAULT)?;
            Ok(format!("续传会话已开启：{path}（{len} 字节）"))
        }
        "uppush" => {
            let img = arg(args, 2)?;
            let path = arg(args, 3)?;
            let file = arg(args, 4)?;
            let off: u64 = args.get(5).map(|v| v.parse()).transpose()?.unwrap_or(0);
            let data = std::fs::read(&file)?;
            let mut s = KosStore::open(open_rw(img, None)?, VerifyLevel::HashOnly)?;
            let (recv, total) = s.upload_push(path, off, &data, 0)?;
            Ok(format!("已推送 {} 字节 @ {off}；进度 {recv}/{total} 块", data.len()))
        }
        "upstat" => {
            let img = arg(args, 2)?;
            let path = arg(args, 3)?;
            let mut s = KosStore::open(open_rw(img, None)?, VerifyLevel::HashOnly)?;
            let (total, recv, cont) = s.upload_status(path, 0)?;
            Ok(format!("{path}：{recv}/{}/{} 块已收（连续前缀 {cont}）",
                total.div_ceil(4096), total.div_ceil(4096)))
        }
        "upfin" => {
            let img = arg(args, 2)?;
            let path = arg(args, 3)?;
            let mut s = KosStore::open(open_rw(img, None)?, VerifyLevel::HashOnly)?;
            s.upload_finish(path, 0)?;
            Ok(format!("{path} 已完成并原子发布"))
        }
        "trunc" => {
            let img = arg(args, 2)?;
            let path = arg(args, 3)?;
            let len: u64 = arg(args, 4)?.parse()?;
            let mut s = KosStore::open(open_rw(img, None)?, VerifyLevel::HashOnly)?;
            s.truncate(path, len, 0)?;
            Ok(format!("{path} 已截断/扩展至 {len} 字节"))
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

fn hex_str(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn parse_hex32(s: &str) -> Result<[u8; 32], Box<dyn std::error::Error>> {
    let s: String = s.chars().filter(|c| *c != ' ' && *c != '\n').collect();
    if s.len() != 64 {
        return Err("种子必须是 64 个十六进制字符".into());
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)?;
    }
    Ok(out)
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
