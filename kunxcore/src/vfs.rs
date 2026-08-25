//! 虚拟文件系统（VFS）：文件描述符 → 打开文件表、挂载表、路径解析，
//! 以及 open/read/write/close/seek 系统调用分发。
//!
//! - 打开文件表把 fd（进程 fd 表中的 `index`）映射到 (设备, inode, 偏移, 标志)。
//! - 挂载表把路径前缀（如 "/"）映射到某个文件系统根 inode。
//! - 常规文件读写分发给 `fs_simple`；字符设备读写按 (major, minor) 分发给 `devfs`。

use crate::devfs;
use crate::fsops::{self, T_CHR, T_DIR, T_REG, T_SYM};
use crate::process::{self, FdKind};

// ==================== 打开标志 ====================

pub const O_RDONLY: u32 = 0;
pub const O_WRONLY: u32 = 1;
pub const O_RDWR: u32 = 2;
pub const O_CREAT: u32 = 0x40;
pub const O_TRUNC: u32 = 0x200;
pub const O_NONBLOCK: u32 = 0x800;

// ==================== seek 起点 ====================

pub const SEEK_SET: u32 = 0;
pub const SEEK_CUR: u32 = 1;
pub const SEEK_END: u32 = 2;

// ==================== 文件类型 ====================

/// 文件类型。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[allow(dead_code)]
pub enum FileType {
    /// 普通文件
    Reg,
    /// 目录
    Dir,
    /// 字符设备
    Chr,
    /// 块设备
    Blk,
    /// 命名管道
    Fifo,
    /// 符号链接
    Sym,
}

impl FileType {
    /// 转为磁盘上的类型码。
    pub fn as_u16(self) -> u16 {
        match self {
            FileType::Reg => fsops::T_REG,
            FileType::Dir => fsops::T_DIR,
            FileType::Chr => fsops::T_CHR,
            FileType::Blk => fsops::T_BLK,
            FileType::Fifo => fsops::T_FIFO,
            FileType::Sym => fsops::T_SYM,
        }
    }

    /// 由磁盘类型码构造。
    #[allow(dead_code)]
    pub fn from_u16(v: u16) -> FileType {
        match v {
            fsops::T_DIR => FileType::Dir,
            fsops::T_CHR => FileType::Chr,
            fsops::T_BLK => FileType::Blk,
            fsops::T_FIFO => FileType::Fifo,
            fsops::T_SYM => FileType::Sym,
            _ => FileType::Reg,
        }
    }
}

// ==================== 打开文件表 ====================

/// 打开文件表容量。
const MAX_OPEN: usize = 32;

/// 一个已打开的文件。
#[derive(Clone, Copy)]
struct OpenFile {
    used: bool,
    /// 文件系统所在块设备 major
    dev: u32,
    /// inode 号
    ino: u32,
    /// 文件类型（去规范化，加速读写分发）
    typ: u16,
    /// 设备号（CHR/BLK 用，编码 (major<<16)|minor）
    rdev: u32,
    /// 文件位置
    offset: usize,
    /// 打开标志
    flags: u32,
    /// 引用计数：指向本表项的进程 fd 数量（fork 复制 +1，close/exit -1，
    /// 归零才释放槽位，防止 fork 后子进程 close 误清新文件槽位）。
    refs: u32,
}

const EMPTY_OPEN: OpenFile = OpenFile {
    used: false,
    dev: 0,
    ino: 0,
    typ: 0,
    rdev: 0,
    offset: 0,
    flags: 0,
    refs: 0,
};

static mut OPEN_FILES: [OpenFile; MAX_OPEN] = [EMPTY_OPEN; MAX_OPEN];

// ==================== 挂载表 ====================

/// 挂载表容量。
const MAX_MOUNT: usize = 8;

/// 一个挂载点。
#[derive(Clone, Copy)]
struct Mount {
    used: bool,
    /// 挂载点路径（如 "/"）
    mount_point: &'static str,
    /// 文件系统所在块设备 major
    dev: u32,
    /// 根 inode 号
    root_ino: u32,
}

const EMPTY_MOUNT: Mount = Mount { used: false, mount_point: "", dev: 0, root_ino: 0 };

static mut MOUNTS: [Mount; MAX_MOUNT] = [EMPTY_MOUNT; MAX_MOUNT];

// ==================== 设备号编解码 ====================

fn make_rdev(major: u32, minor: u32) -> u32 {
    (major << 16) | (minor & 0xFFFF)
}
fn rdev_major(rdev: u32) -> u32 {
    rdev >> 16
}
fn rdev_minor(rdev: u32) -> u32 {
    rdev & 0xFFFF
}

// ==================== 挂载 / 初始化 ====================

/// 把 `dev` 设备的文件系统挂载到 `mount_point`，返回 0；失败 -1。
///
/// 自动探测文件系统类型（SFS / FAT32），注册到分派层后挂载。
pub fn mount(dev: u32, mount_point: &'static str) -> i32 {
    let kind = match fsops::probe(dev) {
        Some(k) => k,
        None => return -1,
    };
    fsops::register(dev, kind);
    if fsops::mount(dev) != 0 {
        return -1;
    }
    let root_ino = fsops::root_ino(dev);
    unsafe {
        for i in 0..MAX_MOUNT {
            if !MOUNTS[i].used {
                MOUNTS[i] = Mount { used: true, mount_point, dev, root_ino };
                return 0;
            }
        }
    }
    -1
}

/// 复位 VFS 打开文件表与挂载表（幂等）。
pub fn init() {
    unsafe {
        for i in 0..MAX_OPEN {
            OPEN_FILES[i] = EMPTY_OPEN;
        }
        for i in 0..MAX_MOUNT {
            MOUNTS[i] = EMPTY_MOUNT;
        }
    }
}

/// 遍历挂载表，对每个挂载点调用 `f(mount_point, dev)`。
pub fn for_each_mount(f: &mut dyn FnMut(&str, u32)) {
    unsafe {
        for i in 0..MAX_MOUNT {
            let m = MOUNTS[i];
            if m.used {
                f(m.mount_point, m.dev);
            }
        }
    }
}

/// 打印挂载表（调试用）。
pub fn print_mounts() {
    unsafe {
        for i in 0..MAX_MOUNT {
            let m = MOUNTS[i];
            if m.used {
                crate::uart_send_str("[vfs] mount ");
                crate::uart_send_str(m.mount_point);
                crate::uart_send_str(" -> major=");
                crate::print_dec(m.dev as u64);
                crate::uart_send_str(" root_ino=");
                crate::print_dec(m.root_ino as u64);
                crate::uart_send_str("\n");
            }
        }
    }
}

// ==================== 路径解析 ====================

/// 找到最长匹配的挂载点，返回 (dev, root_ino, 跳过的字节数)。
fn find_mount(path: &str) -> Option<(u32, u32, usize)> {
    let mut best: Option<(u32, u32, usize)> = None;
    unsafe {
        for i in 0..MAX_MOUNT {
            let m = MOUNTS[i];
            if !m.used {
                continue;
            }
            if path.starts_with(m.mount_point) {
                let rest = &path[m.mount_point.len()..];
                // 根挂载点 "/" 匹配一切路径；其余挂载点需以 "/" 或结束作为边界
                let boundary =
                    m.mount_point == "/" || rest.is_empty() || rest.starts_with('/');
                if boundary && best.map_or(true, |(_, _, l)| m.mount_point.len() > l) {
                    best = Some((m.dev, m.root_ino, m.mount_point.len()));
                }
            }
        }
    }
    best
}

/// 符号链接展开总次数上限（防环路：`a -> b -> a` 等）。
const MAX_SYMLINK_HOPS: u32 = 8;

/// 解析完整路径到 (dev, ino)，全程跟随符号链接（含最后分量，即 `stat` 语义）；
/// 返回 0；失败 -1。
pub fn resolve(path: &str, dev: &mut u32, ino: &mut u32) -> i32 {
    let mut hops = 0u32;
    match resolve_path(path, true, &mut hops) {
        Some((d, i)) => {
            *dev = d;
            *ino = i;
            0
        }
        None => -1,
    }
}

/// 顶层解析：定位挂载点后在根 inode 上逐分量行走。
fn resolve_path(path: &str, follow_final: bool, hops: &mut u32) -> Option<(u32, u32)> {
    let (d, root, skip) = find_mount(path)?;
    walk_from(d, root, &path[skip..], follow_final, hops)
}

/// 从 (dev, dir) 出发按分量走 `path`，遇到符号链接按需展开。
///
/// - `follow_final`：最后分量是符号链接时是否跟随（open/stat 跟随；
///   readlink/创建类路径不跟随）。
/// - `hops`：累计展开次数，超限返回 None（防环路死循环）。
/// - 绝对符号链接目标重新走挂载点解析；相对目标从当前目录继续。
/// - 目录中的 "." / ".." 是真实目录项，天然支持相对目标中的相对行走。
fn walk_from(dev: u32, dir: u32, path: &str, follow_final: bool, hops: &mut u32) -> Option<(u32, u32)> {
    let mut d = dev;
    let mut cur = dir;
    let bytes = path.as_bytes();
    let n = bytes.len();
    let mut pos = 0usize;
    while pos < n {
        while pos < n && bytes[pos] == b'/' {
            pos += 1;
        }
        if pos >= n {
            break;
        }
        let start = pos;
        while pos < n && bytes[pos] != b'/' {
            pos += 1;
        }
        let comp = &path[start..pos];
        if fsops::fs_type(d, cur) != T_DIR {
            return None;
        }
        let next = fsops::fs_lookup(d, cur, comp);
        if next < 0 {
            return None;
        }
        // 判断本分量是否为最后一段（其后只剩分隔符）
        let mut q = pos;
        while q < n && bytes[q] == b'/' {
            q += 1;
        }
        let is_final = q >= n;
        let nt = fsops::fs_type(d, next as u32);
        if nt == T_SYM && (follow_final || !is_final) {
            if *hops >= MAX_SYMLINK_HOPS {
                return None;
            }
            *hops += 1;
            let mut tbuf = [0u8; 256];
            let tn = fsops::fs_read(d, next as u32, 0, &mut tbuf);
            if tn <= 0 {
                return None;
            }
            let target = core::str::from_utf8(&tbuf[..tn as usize]).ok()?;
            if target.starts_with('/') {
                // 绝对目标：重新走挂载点（可能跨 FS），再继续原路径剩余分量
                let (td, ti) = resolve_path(target, false, hops)?;
                d = td;
                cur = ti;
            } else {
                // 相对目标：从当前目录继续走 target，再继续原路径剩余分量
                let (td, ti) = walk_from(d, cur, target, true, hops)?;
                d = td;
                cur = ti;
            }
            pos = q;
            continue;
        }
        cur = next as u32;
        pos = q;
    }
    Some((d, cur))
}

/// 解析路径的父目录，返回 (dev, 父目录 ino, 最后一段名字)；失败 None。
///
/// 目录部分经 `resolve` 解析（跟随符号链接），因此可在符号链接指向的
/// 目录下创建/删除/改名文件。最后一段本身不跟随（创建/删除语义）。
fn resolve_parent(path: &str) -> Option<(u32, u32, &str)> {
    let bytes = path.as_bytes();
    let n = bytes.len();
    // 去掉末尾的 '/'（"/a/b/" 与 "/a/b" 等价；纯 "/" 无父目录）
    let mut end = n;
    while end > 0 && bytes[end - 1] == b'/' {
        end -= 1;
    }
    if end == 0 {
        return None;
    }
    // 找最后一个 '/'（slash 停在最后一个 '/' 之后的位置）
    let mut slash = end;
    while slash > 0 && bytes[slash - 1] != b'/' {
        slash -= 1;
    }
    let name = &path[slash..end];
    let dir = if slash == 0 { "/" } else { &path[..slash] };
    let mut d = 0u32;
    let mut i = 0u32;
    if resolve(dir, &mut d, &mut i) != 0 {
        return None;
    }
    Some((d, i, name))
}

// ==================== 打开文件表 ====================

fn alloc_openfile() -> usize {
    unsafe {
        for i in 0..MAX_OPEN {
            if !OPEN_FILES[i].used {
                OPEN_FILES[i].used = true;
                return i;
            }
        }
    }
    MAX_OPEN // 表满：返回哨兵（调用方据此返回错误，避免 panic DoS）
}

fn open_file(dev: u32, ino: u32, typ: u16, rdev: u32, flags: u32) -> i32 {
    let slot = alloc_openfile();
    if slot >= MAX_OPEN {
        return -1;
    }
    unsafe {
        OPEN_FILES[slot] =
            OpenFile { used: true, dev, ino, typ, rdev, offset: 0, flags, refs: 1 };
    }
    let fd = process::alloc_fd(FdKind::File, slot, flags);
    if fd < 0 {
        // fd 表满：回滚已占用的打开文件表槽位，避免每次失败 open 永久泄漏槽位
        // （32 槽耗尽后所有进程 open 全部失败）
        close_ref(slot);
        return -1;
    }
    fd
}

/// 增加打开文件表项的引用计数（fork 复制 fd 时调用）。
pub fn open_ref(index: usize) {
    if index < MAX_OPEN {
        unsafe {
            let of = &mut OPEN_FILES[index];
            if of.used {
                of.refs = of.refs.saturating_add(1);
            }
        }
    }
}

/// 释放一个打开文件表项的引用（进程退出清理 fd 时调用），归零才释放槽位。
pub fn close_ref(index: usize) {
    if index >= MAX_OPEN {
        return;
    }
    unsafe {
        let of = &mut OPEN_FILES[index];
        if !of.used {
            return;
        }
        if of.refs > 0 {
            of.refs -= 1;
        }
        if of.refs == 0 {
            *of = EMPTY_OPEN;
        }
    }
}

/// 检查 `(dev, ino)` 是否仍被任何打开文件表项引用。
///
/// 供 `unlink` 使用：被打开的文件不可删除（否则 inode 号释放后会被新文件
/// 复用，旧 fd 与新文件形成逻辑别名，读写互相污染）。
pub fn is_open(dev: u32, ino: u32) -> bool {
    unsafe {
        for i in 0..MAX_OPEN {
            let of = OPEN_FILES[i];
            if of.used && of.dev == dev && of.ino == ino {
                return true;
            }
        }
    }
    false
}

// ==================== 系统调用 ====================

/// 打开文件，返回 fd；失败 -1。
///
/// 不存在且带 `O_CREAT` 时创建普通文件；带 `O_TRUNC` 时先截断。
pub fn open(path: &str, flags: u32) -> i32 {
    let mut dev = 0u32;
    let mut ino = 0u32;
    if resolve(path, &mut dev, &mut ino) == 0 {
        let typ = fsops::fs_type(dev, ino);
        if typ == T_DIR && (flags & (O_WRONLY | O_RDWR)) != 0 {
            return -1; // 目录不可写
        }
        if typ == T_REG && (flags & O_TRUNC) != 0 {
            fsops::fs_truncate(dev, ino);
        }
        return open_file(dev, ino, typ, fsops::fs_rdev(dev, ino), flags);
    }
    if flags & O_CREAT != 0 {
        if let Some((d, pino, name)) = resolve_parent(path) {
            let new_ino = fsops::fs_create(d, pino, name, T_REG, 0);
            if new_ino >= 0 {
                return open_file(d, new_ino as u32, T_REG, 0, flags);
            }
        }
    }
    -1
}

/// 读取文件，返回字节数；失败 -1。
pub fn read(fd: i32, buf: &mut [u8]) -> i32 {
    let entry = match process::get_fd(fd) {
        Some(e) if e.kind == FdKind::File => e,
        _ => return -1,
    };
    let of = unsafe { OPEN_FILES[entry.index] };
    if !of.used {
        return -1;
    }
    if (of.flags & 0x3) == O_WRONLY {
        return -1; // 只写不可读
    }
    let n = match of.typ {
        T_CHR => devfs::chr_read(rdev_major(of.rdev), rdev_minor(of.rdev), of.flags, of.offset, buf),
        T_REG => fsops::fs_read(of.dev, of.ino, of.offset, buf),
        _ => -1,
    };
    if n > 0 {
        unsafe {
            OPEN_FILES[entry.index].offset += n as usize;
        }
    }
    n
}

/// 写入文件，返回字节数；失败 -1。
pub fn write(fd: i32, buf: &[u8]) -> i32 {
    let entry = match process::get_fd(fd) {
        Some(e) if e.kind == FdKind::File => e,
        _ => return -1,
    };
    let of = unsafe { OPEN_FILES[entry.index] };
    if !of.used {
        return -1;
    }
    if (of.flags & 0x3) == O_RDONLY {
        return -1; // 只读不可写
    }
    let n = match of.typ {
        T_CHR => devfs::chr_write(rdev_major(of.rdev), rdev_minor(of.rdev), buf),
        T_REG => fsops::fs_write(of.dev, of.ino, of.offset, buf),
        _ => -1,
    };
    if n > 0 {
        unsafe {
            OPEN_FILES[entry.index].offset += n as usize;
        }
    }
    n
}

/// 关闭文件，返回 0；失败 -1。
pub fn close(fd: i32) -> i32 {
    let entry = match process::get_fd(fd) {
        Some(e) if e.kind == FdKind::File => e,
        _ => return -1,
    };
    close_ref(entry.index);
    process::close_fd(fd);
    0
}

/// 向已打开的设备文件下发 ioctl，返回输出字节数；非设备或未处理返回 -25（ENOTTY）。
pub fn ioctl(fd: i32, request: u32, out: &mut [u8]) -> i32 {
    let entry = match process::get_fd(fd) {
        Some(e) if e.kind == FdKind::File => e,
        _ => return -25,
    };
    let of = unsafe { OPEN_FILES[entry.index] };
    if !of.used || of.typ != T_CHR {
        return -25;
    }
    devfs::chr_ioctl(rdev_major(of.rdev), rdev_minor(of.rdev), request, out)
}

/// 把已打开文件截断为 0（释放全部数据块），返回 0；失败 -1。
///
/// 供 `ftruncate` 使用（displayd/networkd 用其清空命令管道文件）。
pub fn truncate(fd: i32) -> i32 {
    let entry = match process::get_fd(fd) {
        Some(e) if e.kind == FdKind::File => e,
        _ => return -1,
    };
    let of = unsafe { OPEN_FILES[entry.index] };
    if !of.used || of.typ != T_REG {
        return -1;
    }
    fsops::fs_truncate(of.dev, of.ino)
}

/// 移动文件位置，返回新位置；失败 -1。
///
/// `offset` 为 64 位有符号（Linux 语义）：不再截断为 i32，支持大文件定位。
pub fn seek(fd: i32, offset: i64, whence: u32) -> i64 {
    let entry = match process::get_fd(fd) {
        Some(e) if e.kind == FdKind::File => e,
        _ => return -1,
    };
    let of = unsafe { OPEN_FILES[entry.index] };
    if !of.used {
        return -1;
    }
    let base = match whence {
        SEEK_SET => 0i64,
        SEEK_CUR => of.offset as i64,
        SEEK_END => fsops::fs_size(of.dev, of.ino) as i64,
        _ => return -1,
    };
    // checked 加法防回绕（offset 可为任意 i64）
    let Some(new) = base.checked_add(offset) else { return -1 };
    if new < 0 {
        return -1;
    }
    unsafe {
        OPEN_FILES[entry.index].offset = new as usize;
    }
    new
}

/// 删除文件，返回 0；失败 -1。
pub fn unlink(path: &str) -> i32 {
    let Some((d, pino, name)) = resolve_parent(path) else { return -1 };
    // 仍被打开的文件不可删除：inode 号被释放后会被 fs_create 复用，旧 fd 将与
    // 新文件别名（H-6 修复的延伸）。拒绝删除（EBUSY 语义）。
    let ino = fsops::fs_lookup(d, pino, name);
    if ino >= 0 {
        let ino = ino as u32;
        if fsops::fs_type(d, ino) != fsops::T_DIR && is_open(d, ino) {
            return -1;
        }
    }
    fsops::fs_unlink(d, pino, name)
}

/// 重命名（移动）文件：`old_path` → `new_path`，可跨目录（SFS 迁移同一 inode）。
/// 目标已存在或源为目录时失败。返回 0；失败 -1。
pub fn rename(old_path: &str, new_path: &str) -> i32 {
    let Some((od, oi, on)) = resolve_parent(old_path) else { return -1 };
    let Some((_nd, ni, nn)) = resolve_parent(new_path) else { return -1 };
    fsops::fs_rename(od, oi, on, ni, nn)
}

/// 创建设备文件（或 FIFO），返回 0；失败 -1。
pub fn mknod(path: &str, typ: FileType, major: u32, minor: u32) -> i32 {
    let Some((d, pino, name)) = resolve_parent(path) else { return -1 };
    let ino = fsops::fs_create(d, pino, name, typ.as_u16(), make_rdev(major, minor));
    if ino < 0 {
        -1
    } else {
        0
    }
}

/// 创建符号链接：`target` 指向 `path`，返回 0；失败 -1。
///
/// 目录部分跟随符号链接（可在链接指向的目录下创建）；最后一段已存在则失败。
pub fn symlink(target: &str, path: &str) -> i32 {
    let Some((d, pino, name)) = resolve_parent(path) else { return -1 };
    let ino = fsops::fs_symlink(d, pino, name, target);
    if ino < 0 {
        -1
    } else {
        0
    }
}

/// 读取符号链接 `path` 的目标字符串（不跟随），写入 `buf`，返回字节数；失败 -1。
///
/// 目标非符号链接返回 -1（EINVAL 语义）；`buf` 不足时截断（Linux 语义）。
pub fn readlink(path: &str, buf: &mut [u8]) -> i32 {
    let Some((d, pino, name)) = resolve_parent(path) else { return -1 };
    let ino = fsops::fs_lookup(d, pino, name);
    if ino < 0 {
        return -1;
    }
    let ino = ino as u32;
    if fsops::fs_type(d, ino) != fsops::T_SYM {
        return -1;
    }
    fsops::fs_readlink(d, ino, buf)
}

/// 创建目录，返回 0；失败 -1。
pub fn mkdir(path: &str) -> i32 {
    let Some((d, pino, name)) = resolve_parent(path) else { return -1 };
    let ino = fsops::fs_mkdir(d, pino, name);
    if ino < 0 {
        -1
    } else {
        0
    }
}

/// 读取已打开文件的大小（字节），失败返回 -1。
#[allow(dead_code)]
pub fn file_size(fd: i32) -> i64 {
    let entry = match process::get_fd(fd) {
        Some(e) if e.kind == FdKind::File => e,
        _ => return -1,
    };
    let of = unsafe { OPEN_FILES[entry.index] };
    if !of.used {
        return -1;
    }
    fsops::fs_size(of.dev, of.ino) as i64
}

/// 读取目录第 `index` 个条目的名字，返回 inode 号；结束返回 -1。
pub fn readdir(path: &str, index: usize, name: &mut [u8]) -> i32 {
    let mut dev = 0u32;
    let mut ino = 0u32;
    if resolve(path, &mut dev, &mut ino) != 0 {
        return -1;
    }
    fsops::fs_readdir(dev, ino, index, name)
}

/// 读取已打开目录的下一个条目（以打开文件的 `offset` 记录目录流位置），
/// 返回 (inode 号, 条目类型码)；结束或非目录返回 (-1, 0)。
///
/// 与 Linux 的「目录即流」语义一致：`getdents64` 连续调用会自动前进位置。
pub fn readdir_next(fd: i32, name: &mut [u8]) -> (i32, u16) {
    let entry = match process::get_fd(fd) {
        Some(e) if e.kind == FdKind::File => e,
        _ => return (-1, 0),
    };
    let mut of = unsafe { OPEN_FILES[entry.index] };
    if !of.used || of.typ != T_DIR {
        return (-1, 0);
    }
    let idx = of.offset;
    let ino = fsops::fs_readdir(of.dev, of.ino, idx, name);
    if ino < 0 {
        return (-1, 0);
    }
    of.offset = idx + 1;
    unsafe { OPEN_FILES[entry.index] = of };
    (ino, fsops::fs_type(of.dev, ino as u32))
}

/// 返回已打开文件的 (类型码, 大小)，失败返回 None。
///
/// 类型码为 `fs_simple` 的 T_REG/T_DIR/T_CHR/T_BLK/T_FIFO，供 `fstat` 翻译。
pub fn fstat(fd: i32) -> Option<(u16, u32)> {
    let entry = process::get_fd(fd)?;
    if entry.kind != FdKind::File {
        return None;
    }
    let of = unsafe { OPEN_FILES[entry.index] };
    if !of.used {
        return None;
    }
    Some((of.typ, fsops::fs_size(of.dev, of.ino)))
}

/// 由路径直接获取 (类型码, 大小)，不建立打开文件表项，失败返回 None。
///
/// 供 `newfstatat` 翻译使用（无需真正 open/close）。
pub fn stat_path(path: &str) -> Option<(u16, u32)> {
    let mut dev = 0u32;
    let mut ino = 0u32;
    if resolve(path, &mut dev, &mut ino) != 0 {
        return None;
    }
    Some((fsops::fs_type(dev, ino), fsops::fs_size(dev, ino)))
}

/// 由路径直接获取 (类型码, 大小)，**不跟随最后分量**（`lstat` 语义）。
///
/// 供 `ls -l` 显示符号链接自身及其目标。
pub fn lstat_path(path: &str) -> Option<(u16, u32)> {
    let Some((d, pino, name)) = resolve_parent(path) else { return None };
    let ino = fsops::fs_lookup(d, pino, name);
    if ino < 0 {
        return None;
    }
    let ino = ino as u32;
    Some((fsops::fs_type(d, ino), fsops::fs_size(d, ino)))
}

/// 返回已打开文件的设备号 `rdev`（编码 `(major<<16)|minor`）；非设备返回 None。
///
/// 供 `mmap` 判断 fd 是否为帧缓冲设备（`devfs::FB_MAJOR`）。
pub fn open_rdev(fd: i32) -> Option<u32> {
    let entry = process::get_fd(fd)?;
    if entry.kind != FdKind::File {
        return None;
    }
    let of = unsafe { OPEN_FILES[entry.index] };
    if !of.used {
        return None;
    }
    Some(of.rdev)
}
