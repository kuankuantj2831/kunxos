//! 简单文件系统：超级块 + inode + 数据块，目录 = (inode 号, 文件名)。
//!
//! 磁盘布局（块大小 4KB，块号从 0 开始）：
//!   ```text
//!   块 0        超级块
//!   块 1        inode 位图（每 inode 1 位，共 128 个 inode）
//!   块 2        数据块位图（每数据块 1 位）
//!   块 3..10    inode 表（128 个 inode × 256 字节 = 8 块）
//!   块 11..N-1  数据块
//!   ```
//!
//! inode 磁盘布局（256 字节）：
//!   ```text
//!   type@0(u16)  nlink@2(u16)  size@4(u32)  rdev@8(u32)
//!   direct@12（48 个 u32 块号，单文件最大 192KB）
//!   ```
//!
//! 目录项（32 字节）：ino@0(u32) + name@4（28 字节，以 \0 结尾）。

use crate::blkdev::{self, BLK_SIZE};

/// inode 类型码。
pub const T_REG: u16 = 1;
pub const T_DIR: u16 = 2;
pub const T_CHR: u16 = 3;
pub const T_BLK: u16 = 4;
pub const T_FIFO: u16 = 5;
/// 符号链接：目标路径作为文件数据存储（`fs_read`/`fs_write` 天然支持）。
pub const T_SYM: u16 = 6;

/// 最大 inode 数。
///
/// 64 个在首次启动复制 OOBE 默认配置（16 文件）到 /data 后即耗尽，
/// 导致后续 mkdir/写文件全部失败；128 个（inode 表 4 块）可容纳
/// rootfs + OOBE 默认配置 + 用户创建的文件，同时仍留有充足数据块。
const MAX_INODES: usize = 128;
/// inode 大小（字节）：容纳 48 个直接块号（12 + 48×4 = 204 ≤ 256）。
const INODE_SIZE: usize = 128;
/// 每个 inode 表块容纳的 inode 数。
const INODES_PER_BLOCK: usize = BLK_SIZE / INODE_SIZE; // 32
/// inode 表块数。
const INODE_BLOCKS: usize = MAX_INODES.div_ceil(INODES_PER_BLOCK); // 2
/// 直接块数量（单文件最大 96KB）。
const NDIRECT: usize = 24;
/// 文件名最大长度（不含结尾 \0）。
const MAX_NAME: usize = 27;
/// 目录项大小。
const DIRENT_SIZE: usize = 32;

/// 各区域起始块号。
const SUPER_BLOCK: usize = 0;
const IMAP_BLOCK: usize = 1;
const BMAP_BLOCK: usize = 2;
const INODE_START: usize = 3;
const DATA_START: usize = INODE_START + INODE_BLOCKS; // 5

/// 超级块魔数。
const MAGIC: u32 = 0x5F_53_46_53; // "_SFS"

// 超级块字段在块 0 内的字节偏移。
const SB_MAGIC: usize = 0;
const SB_BLK_SIZE: usize = 4;
const SB_TOTAL: usize = 8;
const SB_INODE_BLOCKS: usize = 12;
const SB_DATA_START: usize = 16;
const SB_NUM_INODES: usize = 20;
const SB_ROOT_INO: usize = 24;
const SB_FREE_BLOCKS: usize = 28;
const SB_FREE_INODES: usize = 32;

// ==================== 字节序列化 ====================

fn put_u16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
}
fn get_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}
fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
fn get_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

// ==================== 块读写 ====================

fn read_block(major: u32, block: usize, buf: &mut [u8]) -> bool {
    blkdev::blk_read(major, block, buf) == BLK_SIZE as i32
}
fn write_block(major: u32, block: usize, buf: &[u8]) -> bool {
    blkdev::blk_write(major, block, buf) == BLK_SIZE as i32
}

// ==================== inode 读写 ====================

fn inode_block(ino: u32) -> usize {
    INODE_START + ino as usize / INODES_PER_BLOCK
}
fn inode_off(ino: u32) -> usize {
    (ino as usize % INODES_PER_BLOCK) * INODE_SIZE
}

fn read_inode(major: u32, ino: u32, inode: &mut [u8; INODE_SIZE]) -> bool {
    // inode 号越界会读写到数据块区域（持久化数据损坏），入口统一校验
    if ino as usize >= MAX_INODES {
        return false;
    }
    let mut buf = [0u8; BLK_SIZE];
    if !read_block(major, inode_block(ino), &mut buf) {
        return false;
    }
    inode.copy_from_slice(&buf[inode_off(ino)..inode_off(ino) + INODE_SIZE]);
    true
}

fn write_inode(major: u32, ino: u32, inode: &[u8; INODE_SIZE]) -> bool {
    if ino as usize >= MAX_INODES {
        return false;
    }
    let mut buf = [0u8; BLK_SIZE];
    if !read_block(major, inode_block(ino), &mut buf) {
        return false;
    }
    buf[inode_off(ino)..inode_off(ino) + INODE_SIZE].copy_from_slice(inode);
    write_block(major, inode_block(ino), &buf)
}

// ==================== inode 字段访问 ====================

fn inode_type(inode: &[u8; INODE_SIZE]) -> u16 {
    get_u16(inode, 0)
}
fn inode_size(inode: &[u8; INODE_SIZE]) -> u32 {
    get_u32(inode, 4)
}
fn inode_rdev(inode: &[u8; INODE_SIZE]) -> u32 {
    get_u32(inode, 8)
}
fn inode_direct(inode: &[u8; INODE_SIZE], i: usize) -> u32 {
    get_u32(inode, 12 + i * 4)
}

fn inode_set_type(inode: &mut [u8; INODE_SIZE], v: u16) {
    put_u16(inode, 0, v);
}
fn inode_set_nlink(inode: &mut [u8; INODE_SIZE], v: u16) {
    put_u16(inode, 2, v);
}
fn inode_set_size(inode: &mut [u8; INODE_SIZE], v: u32) {
    put_u32(inode, 4, v);
}
fn inode_set_rdev(inode: &mut [u8; INODE_SIZE], v: u32) {
    put_u32(inode, 8, v);
}
fn inode_set_direct(inode: &mut [u8; INODE_SIZE], i: usize, v: u32) {
    put_u32(inode, 12 + i * 4, v);
}

// ==================== 位图 ====================

/// 在 `block` 号块的第 `max` 位内分配一个空闲位，返回位号；无空闲返回 -1。
fn bitmap_alloc(major: u32, block: usize, max: usize) -> i32 {
    let mut buf = [0u8; BLK_SIZE];
    if !read_block(major, block, &mut buf) {
        return -1;
    }
    // 位图容量上限：单块最多 4096*8 位，防止 max 过大时 buf[i/8] 栈越界写
    let max = max.min(BLK_SIZE * 8);
    for i in 0..max {
        if buf[i / 8] & (1 << (i % 8)) == 0 {
            buf[i / 8] |= 1 << (i % 8);
            write_block(major, block, &buf);
            return i as i32;
        }
    }
    -1
}

/// 释放 `block` 号块位图中的第 `index` 位。
fn bitmap_free(major: u32, block: usize, index: usize) {
    if index >= BLK_SIZE * 8 {
        return;
    }
    let mut buf = [0u8; BLK_SIZE];
    if !read_block(major, block, &mut buf) {
        return;
    }
    buf[index / 8] &= !(1 << (index % 8));
    write_block(major, block, &buf);
}

fn inode_alloc(major: u32) -> i32 {
    bitmap_alloc(major, IMAP_BLOCK, MAX_INODES)
}
fn inode_free(major: u32, ino: u32) {
    bitmap_free(major, IMAP_BLOCK, ino as usize);
}

fn total_blocks(major: u32) -> u32 {
    let dev_blocks =
        blkdev::get_blkdev(major).map_or(0, |d| (d.ops.num_sectors / blkdev::SECTORS_PER_BLOCK) as u32);
    // 单块位图最多跟踪 BLK_SIZE*8 个数据块（32768 块 = 128MB 数据）。
    // 设备容量（如 256MB 的 /data 分区）超过位图容量时按位图容量钳制，
    // 使 mkfs 的超级块、df 统计与分配器保持一致，杜绝「统计了位图覆盖
    // 不到的块」的失真（分配本身已被 bitmap_alloc 的 max 钳制保护）。
    dev_blocks.min((DATA_START + BLK_SIZE * 8) as u32)
}

fn data_block_alloc(major: u32) -> i32 {
    let total = total_blocks(major);
    if total as usize <= DATA_START {
        return -1;
    }
    let ndata = total as usize - DATA_START;
    let d = bitmap_alloc(major, BMAP_BLOCK, ndata);
    if d < 0 {
        -1
    } else {
        let blk = DATA_START as i32 + d;
        // 清零新分配块，避免残留已释放文件的数据（跨进程信息泄露）
        let zero = [0u8; BLK_SIZE];
        write_block(major, blk as usize, &zero);
        blk
    }
}

fn data_block_free(major: u32, disk_block: u32) {
    if disk_block < DATA_START as u32 {
        return;
    }
    bitmap_free(major, BMAP_BLOCK, disk_block as usize - DATA_START);
}

// ==================== 块映射 ====================

/// 逻辑块号 → 磁盘块号；未分配或越界返回 0。
fn bmap(inode: &[u8; INODE_SIZE], logical: usize) -> u32 {
    if logical < NDIRECT {
        inode_direct(inode, logical)
    } else {
        0
    }
}

/// 逻辑块号 → 磁盘块号，未分配则分配新数据块；失败返回 0。
fn bmap_alloc(major: u32, inode: &mut [u8; INODE_SIZE], logical: usize) -> u32 {
    if logical >= NDIRECT {
        return 0;
    }
    let existing = inode_direct(inode, logical);
    if existing != 0 {
        return existing;
    }
    let blk = data_block_alloc(major) as u32;
    if blk == 0 {
        return 0;
    }
    inode_set_direct(inode, logical, blk);
    blk
}

// ==================== 目录项 ====================

fn put_name(buf: &mut [u8], off: usize, name: &str) {
    let bytes = name.as_bytes();
    let n = bytes.len().min(MAX_NAME);
    buf[off..off + MAX_NAME + 1].fill(0);
    buf[off..off + n].copy_from_slice(&bytes[..n]);
}

fn name_eq(buf: &[u8], off: usize, name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.len() > MAX_NAME {
        return false;
    }
    for (i, &c) in bytes.iter().enumerate() {
        if buf[off + i] != c {
            return false;
        }
    }
    buf[off + bytes.len()] == 0
}

fn get_name(buf: &[u8], off: usize) -> [u8; MAX_NAME + 1] {
    let mut out = [0u8; MAX_NAME + 1];
    for (i, o) in out.iter_mut().enumerate() {
        let c = buf[off + i];
        if c == 0 {
            break;
        }
        *o = c;
    }
    out
}

/// 在目录 `dir_ino` 中追加一条 (entry_ino, name) 目录项，返回 0；失败 -1。
fn dir_add_entry(major: u32, dir_ino: u32, entry_ino: u32, name: &str) -> i32 {
    let mut inode = [0u8; INODE_SIZE];
    if !read_inode(major, dir_ino, &mut inode) {
        return -1;
    }
    if inode_type(&inode) != T_DIR {
        return -1;
    }
    let size = inode_size(&inode) as usize;

    // 1) 在已分配块中寻找空目录项（ino == 0）
    for i in 0..NDIRECT {
        let blk = inode_direct(&inode, i);
        if blk == 0 {
            break;
        }
        let mut buf = [0u8; BLK_SIZE];
        if !read_block(major, blk as usize, &mut buf) {
            return -1;
        }
        for e in (0..BLK_SIZE).step_by(DIRENT_SIZE) {
            if get_u32(&buf, e) == 0 {
                put_u32(&mut buf, e, entry_ino);
                put_name(&mut buf, e + 4, name);
                write_block(major, blk as usize, &buf);
                return 0;
            }
        }
    }

    // 2) 分配新数据块，写入首条目录项；目录已写满（块已存在或超限）则返回 -1，
    //    避免覆盖已有目录项造成数据丢失
    let logical = size / BLK_SIZE;
    if logical >= NDIRECT || inode_direct(&inode, logical) != 0 {
        return -1;
    }
    let blk = bmap_alloc(major, &mut inode, logical);
    if blk == 0 {
        return -1;
    }
    let mut buf = [0u8; BLK_SIZE];
    put_u32(&mut buf, 0, entry_ino);
    put_name(&mut buf, 4, name);
    write_block(major, blk as usize, &buf);
    inode_set_size(&mut inode, (logical as u32 + 1) * BLK_SIZE as u32);
    write_inode(major, dir_ino, &inode);
    0
}

// ==================== 对外接口 ====================

/// 格式化 `major` 设备为简单文件系统，返回 0；失败 -1。
pub fn mkfs(major: u32) -> i32 {
    let total = total_blocks(major);
    if total as usize <= DATA_START {
        return -1;
    }

    let mut sb = [0u8; BLK_SIZE];
    put_u32(&mut sb, SB_MAGIC, MAGIC);
    put_u32(&mut sb, SB_BLK_SIZE, BLK_SIZE as u32);
    put_u32(&mut sb, SB_TOTAL, total);
    put_u32(&mut sb, SB_INODE_BLOCKS, INODE_BLOCKS as u32);
    put_u32(&mut sb, SB_DATA_START, DATA_START as u32);
    put_u32(&mut sb, SB_NUM_INODES, MAX_INODES as u32);
    put_u32(&mut sb, SB_ROOT_INO, 1);
    put_u32(&mut sb, SB_FREE_BLOCKS, total - DATA_START as u32);
    put_u32(&mut sb, SB_FREE_INODES, (MAX_INODES - 1) as u32);
    if !write_block(major, SUPER_BLOCK, &sb) {
        return -1;
    }

    // 清零 inode 位图与数据块位图，并保留 inode 0（作为“无 inode”哨兵）
    let zero = [0u8; BLK_SIZE];
    write_block(major, IMAP_BLOCK, &zero);
    write_block(major, BMAP_BLOCK, &zero);
    let mut imap = [0u8; BLK_SIZE];
    imap[0] = 1; // bit 0 置位，保留 inode 0
    write_block(major, IMAP_BLOCK, &imap);

    // 清零 inode 表块：新盘上未分配的 inode 槽必须为空（此前不清零，
    // `alloc_pages_contig` 返回的残留 RAM 可能被损坏盘/异常路径当作
    // 垃圾 T_DIR + 垃圾块号处理）
    for b in 0..INODE_BLOCKS {
        write_block(major, INODE_START + b, &zero);
    }

    // 分配根 inode（DIR，编号 1），含 "." 与 ".."
    let root_ino = inode_alloc(major);
    if root_ino != 1 {
        return -1;
    }
    let mut root = [0u8; INODE_SIZE];
    inode_set_type(&mut root, T_DIR);
    inode_set_nlink(&mut root, 1);
    inode_set_size(&mut root, 0);
    write_inode(major, root_ino as u32, &root);
    dir_add_entry(major, root_ino as u32, root_ino as u32, ".");
    dir_add_entry(major, root_ino as u32, root_ino as u32, "..");
    0
}

/// 检查 `major` 设备是否带 SFS 超级块魔数（不执行挂载）。
pub fn fs_probe(major: u32) -> bool {
    let mut sb = [0u8; BLK_SIZE];
    if !read_block(major, SUPER_BLOCK, &mut sb) {
        return false;
    }
    get_u32(&sb, SB_MAGIC) == MAGIC
}

/// 挂载 `major` 设备：校验超级块魔数，返回 0；失败 -1。
pub fn mount(major: u32) -> i32 {
    let mut sb = [0u8; BLK_SIZE];
    if !read_block(major, SUPER_BLOCK, &mut sb) {
        return -1;
    }
    if get_u32(&sb, SB_MAGIC) != MAGIC {
        return -1;
    }
    0
}

/// 根 inode 号（从超级块读取）。
pub fn fs_root_ino(major: u32) -> u32 {
    let mut sb = [0u8; BLK_SIZE];
    if !read_block(major, SUPER_BLOCK, &mut sb) {
        return 0;
    }
    get_u32(&sb, SB_ROOT_INO)
}

/// 在目录 `dir_ino` 中查找 `name`，返回目标 inode 号；未找到 -1。
pub fn fs_lookup(major: u32, dir_ino: u32, name: &str) -> i32 {
    if name.is_empty() || name.len() > MAX_NAME {
        return -1;
    }
    let mut inode = [0u8; INODE_SIZE];
    if !read_inode(major, dir_ino, &mut inode) {
        return -1;
    }
    if inode_type(&inode) != T_DIR {
        return -1;
    }
    for i in 0..NDIRECT {
        let blk = inode_direct(&inode, i);
        if blk == 0 {
            break;
        }
        let mut buf = [0u8; BLK_SIZE];
        if !read_block(major, blk as usize, &mut buf) {
            return -1;
        }
        for e in (0..BLK_SIZE).step_by(DIRENT_SIZE) {
            let ino = get_u32(&buf, e);
            if ino != 0 && name_eq(&buf, e + 4, name) {
                return ino as i32;
            }
        }
    }
    -1
}

/// 在目录 `dir_ino` 中创建类型为 `typ` 的新 inode（名为 `name`），返回 inode 号；失败 -1。
pub fn fs_create(major: u32, dir_ino: u32, name: &str, typ: u16, rdev: u32) -> i32 {
    if name.is_empty() || name.len() > MAX_NAME {
        return -1;
    }
    if fs_lookup(major, dir_ino, name) >= 0 {
        return -1; // 已存在
    }
    let ino = inode_alloc(major);
    if ino < 0 {
        return -1;
    }
    let mut inode = [0u8; INODE_SIZE];
    inode_set_type(&mut inode, typ);
    inode_set_nlink(&mut inode, 1);
    inode_set_size(&mut inode, 0);
    inode_set_rdev(&mut inode, rdev);
    // 写 inode 失败：回滚位图，避免 inode 泄漏
    if !write_inode(major, ino as u32, &inode) {
        inode_free(major, ino as u32);
        return -1;
    }

    if dir_add_entry(major, dir_ino, ino as u32, name) != 0 {
        inode_free(major, ino as u32);
        return -1;
    }
    ino
}

/// 在目录 `dir_ino` 下创建名为 `name` 的符号链接，目标为 `target`，
/// 返回 inode 号；失败 -1。
///
/// 目标路径作为 T_SYM inode 的文件数据存储（无独立盘格式变化），
/// 读写复用 `fs_write`/`fs_read`。目标超长（>240 字节）拒绝。
pub fn fs_symlink(major: u32, dir_ino: u32, name: &str, target: &str) -> i32 {
    if name.is_empty() || name.len() > MAX_NAME || target.len() > 240 {
        return -1;
    }
    let ino = fs_create(major, dir_ino, name, T_SYM, 0);
    if ino < 0 {
        return ino;
    }
    // 写入目标字符串；失败回滚（摘目录项 + 释放 inode/数据块），不留半成品
    if fs_write(major, ino as u32, 0, target.as_bytes()) != target.len() as i32 {
        dir_remove_entry(major, dir_ino, name);
        fs_truncate(major, ino as u32);
        inode_free(major, ino as u32);
        return -1;
    }
    ino
}

/// 读取符号链接 `ino` 的目标字符串（不跟随），写入 `buf`，返回字节数；失败 -1。
pub fn fs_readlink(major: u32, ino: u32, buf: &mut [u8]) -> i32 {
    let size = fs_size(major, ino) as usize;
    if size == 0 {
        return 0;
    }
    let n = size.min(buf.len());
    fs_read(major, ino, 0, &mut buf[..n])
}

/// 创建目录，返回 inode 号；失败 -1。
pub fn fs_mkdir(major: u32, dir_ino: u32, name: &str) -> i32 {
    let ino = fs_create(major, dir_ino, name, T_DIR, 0);
    if ino < 0 {
        return ino;
    }
    // 添加 "." / ".." 失败（如目录块写满）时释放已分配的目录 inode 并
    // 从父目录删除目录项，避免留下半成品目录（inode 泄漏 + 无法删除）
    if dir_add_entry(major, ino as u32, ino as u32, ".") != 0
        || dir_add_entry(major, ino as u32, dir_ino, "..") != 0
    {
        dir_remove_entry(major, dir_ino, name);
        inode_free(major, ino as u32);
        return -1;
    }
    ino
}

/// 从 inode `ino` 的 `offset` 处读取，返回实际读到的字节数；失败 -1。
pub fn fs_read(major: u32, ino: u32, offset: usize, buf: &mut [u8]) -> i32 {
    let mut inode = [0u8; INODE_SIZE];
    if !read_inode(major, ino, &mut inode) {
        return -1;
    }
    let size = inode_size(&inode) as usize;
    if offset >= size {
        return 0;
    }
    let mut total = 0usize;
    let mut pos = offset;
    while total < buf.len() && pos < size {
        let logical = pos / BLK_SIZE;
        let blk = bmap(&inode, logical);
        let n = (buf.len() - total).min(BLK_SIZE - pos % BLK_SIZE).min(size - pos);
        if blk == 0 {
            // 空洞：按零填充
            buf[total..total + n].fill(0);
        } else {
            let mut tmp = [0u8; BLK_SIZE];
            if !read_block(major, blk as usize, &mut tmp) {
                break;
            }
            let off = pos % BLK_SIZE;
            buf[total..total + n].copy_from_slice(&tmp[off..off + n]);
        }
        total += n;
        pos += n;
    }
    total as i32
}

/// 从 inode `ino` 的 `offset` 处写入，返回实际写入字节数；失败 -1。
pub fn fs_write(major: u32, ino: u32, offset: usize, buf: &[u8]) -> i32 {
    let mut inode = [0u8; INODE_SIZE];
    if !read_inode(major, ino, &mut inode) {
        return -1;
    }
    if inode_type(&inode) == T_DIR {
        return -1;
    }
    let mut total = 0usize;
    let mut pos = offset;
    let mut limit_hit = false;
    while total < buf.len() {
        let logical = pos / BLK_SIZE;
        if logical >= NDIRECT {
            // 超出单文件上限（NDIRECT×BLK_SIZE = 192KB）
            limit_hit = true;
            break;
        }
        let blk = bmap_alloc(major, &mut inode, logical);
        if blk == 0 {
            break;
        }
        let off = pos % BLK_SIZE;
        let n = (buf.len() - total).min(BLK_SIZE - off);
        if off == 0 && n == BLK_SIZE {
            let mut tmp = [0u8; BLK_SIZE];
            tmp.copy_from_slice(&buf[total..total + BLK_SIZE]);
            if !write_block(major, blk as usize, &tmp) {
                break;
            }
        } else {
            let mut tmp = [0u8; BLK_SIZE];
            // 读-改-写：读失败必须中止，否则 tmp 未初始化会清掉整块数据
            if !read_block(major, blk as usize, &mut tmp) {
                break;
            }
            tmp[off..off + n].copy_from_slice(&buf[total..total + n]);
            if !write_block(major, blk as usize, &tmp) {
                break;
            }
        }
        total += n;
        pos += n;
        if pos > inode_size(&inode) as usize {
            inode_set_size(&mut inode, pos as u32);
        }
    }
    // 无论部分成功还是失败都回写 inode（记录已分配块与最新大小），
    // 避免块分配成功但 inode 未持久化导致块泄漏
    write_inode(major, ino, &inode);
    // 写入位置超出单文件上限（192KB）且一个字节都没写成 → 报错（-1），
    // 而不是静默返回 0（此前 offset≥192KB 的写看起来「成功写了 0 字节」）
    if total == 0 && limit_hit {
        return -1;
    }
    total as i32
}

/// 把 inode `ino` 截断为 0（释放全部数据块），返回 0；失败 -1。
pub fn fs_truncate(major: u32, ino: u32) -> i32 {
    let mut inode = [0u8; INODE_SIZE];
    if !read_inode(major, ino, &mut inode) {
        return -1;
    }
    // 遍历全部直接块（遇空洞不提前 break，避免稀疏文件的尾部块泄漏）
    for i in 0..NDIRECT {
        let blk = inode_direct(&inode, i);
        if blk != 0 {
            data_block_free(major, blk);
            inode_set_direct(&mut inode, i, 0);
        }
    }
    inode_set_size(&mut inode, 0);
    write_inode(major, ino, &inode);
    0
}

/// 内部：从目录 `dir_ino` 中删除名字对应的目录项（不检查目标类型），
/// 返回被删除的 inode 号；未找到返回 -1。
fn dir_remove_entry(major: u32, dir_ino: u32, name: &str) -> i32 {
    let mut dinode = [0u8; INODE_SIZE];
    if !read_inode(major, dir_ino, &mut dinode) {
        return -1;
    }
    if inode_type(&dinode) != T_DIR {
        return -1;
    }
    for i in 0..NDIRECT {
        let blk = inode_direct(&dinode, i);
        if blk == 0 {
            break;
        }
        let mut buf = [0u8; BLK_SIZE];
        if !read_block(major, blk as usize, &mut buf) {
            return -1;
        }
        for e in (0..BLK_SIZE).step_by(DIRENT_SIZE) {
            let ino = get_u32(&buf, e);
            if ino != 0 && name_eq(&buf, e + 4, name) {
                put_u32(&mut buf, e, 0);
                write_block(major, blk as usize, &buf);
                return ino as i32;
            }
        }
    }
    -1
}

/// 从目录 `dir_ino` 中删除 `name`，并释放其 inode 与数据块，返回 0；失败 -1。
pub fn fs_unlink(major: u32, dir_ino: u32, name: &str) -> i32 {
    // 拒绝空名、"."/".."（防止截断释放根 inode / 目录自身）
    if name.is_empty() || name.len() > MAX_NAME || name == "." || name == ".." {
        return -1;
    }
    let mut dinode = [0u8; INODE_SIZE];
    if !read_inode(major, dir_ino, &mut dinode) {
        return -1;
    }
    if inode_type(&dinode) != T_DIR {
        return -1;
    }
    for i in 0..NDIRECT {
        let blk = inode_direct(&dinode, i);
        if blk == 0 {
            break;
        }
        let mut buf = [0u8; BLK_SIZE];
        if !read_block(major, blk as usize, &mut buf) {
            return -1;
        }
        for e in (0..BLK_SIZE).step_by(DIRENT_SIZE) {
            let ino = get_u32(&buf, e);
            if ino != 0 && name_eq(&buf, e + 4, name) {
                // 拒绝删除目录（目录删除应走独立的 rmdir 语义）
                if fs_type(major, ino) == T_DIR {
                    return -1;
                }
                put_u32(&mut buf, e, 0);
                write_block(major, blk as usize, &buf);
                fs_truncate(major, ino);
                inode_free(major, ino);
                return 0;
            }
        }
    }
    -1
}

/// 重命名：把 `old_dir` 下的 `old_name` 移到 `new_dir` 下的 `new_name`。
///
/// 仅支持普通文件：目录的 "." / ".." 一致性无法在简单 FS 中安全维护，
/// 设备节点同样拒绝。目标已存在时拒绝（不做覆盖）。失败时回滚旧目录项。
/// 成功返回 0；失败 -1。
pub fn fs_rename(major: u32, old_dir: u32, old_name: &str, new_dir: u32, new_name: &str) -> i32 {
    // 参数校验：空名 / 超长名 / "." / ".."
    if old_name.is_empty()
        || old_name.len() > MAX_NAME
        || new_name.is_empty()
        || new_name.len() > MAX_NAME
        || old_name == "."
        || old_name == ".."
        || new_name == "."
        || new_name == ".."
    {
        return -1;
    }
    // 同名同目录：空操作
    if old_dir == new_dir && old_name == new_name {
        return 0;
    }
    // 源必须存在且为普通文件或符号链接
    let ino = fs_lookup(major, old_dir, old_name);
    if ino < 0 {
        return -1;
    }
    let ityp = fs_type(major, ino as u32);
    if ityp != T_REG && ityp != T_SYM {
        return -1;
    }
    // 目标已存在：拒绝（避免覆盖数据）
    if fs_lookup(major, new_dir, new_name) >= 0 {
        return -1;
    }
    // 先从旧目录摘除，再写入新目录；写入失败则回滚（旧目录刚腾出空位，恢复必成功）
    if dir_remove_entry(major, old_dir, old_name) < 0 {
        return -1;
    }
    if dir_add_entry(major, new_dir, ino as u32, new_name) != 0 {
        dir_add_entry(major, old_dir, ino as u32, old_name);
        return -1;
    }
    0
}

/// 读取目录第 `index` 个条目，返回其 inode 号并把名字写入 `name`；结束返回 -1。
pub fn fs_readdir(major: u32, ino: u32, index: usize, name: &mut [u8]) -> i32 {
    let mut inode = [0u8; INODE_SIZE];
    if !read_inode(major, ino, &mut inode) {
        return -1;
    }
    if inode_type(&inode) != T_DIR {
        return -1;
    }
    let mut seen = 0usize;
    for i in 0..NDIRECT {
        let blk = inode_direct(&inode, i);
        if blk == 0 {
            break;
        }
        let mut buf = [0u8; BLK_SIZE];
        if !read_block(major, blk as usize, &mut buf) {
            return -1;
        }
        for e in (0..BLK_SIZE).step_by(DIRENT_SIZE) {
            let eino = get_u32(&buf, e);
            if eino == 0 {
                continue;
            }
            if seen == index {
                let nm = get_name(&buf, e + 4);
                let len = nm.iter().position(|&c| c == 0).unwrap_or(nm.len());
                let n = len.min(name.len());
                name[..n].copy_from_slice(&nm[..n]);
                if n < name.len() {
                    name[n] = 0;
                }
                return eino as i32;
            }
            seen += 1;
        }
    }
    -1
}

/// 空闲数据块数（实时扫描数据块位图，不依赖超级块里的陈旧计数）。
pub fn fs_free_blocks(major: u32) -> u32 {
    let mut buf = [0u8; BLK_SIZE];
    if !read_block(major, BMAP_BLOCK, &mut buf) {
        return 0;
    }
    let total = total_blocks(major);
    if total as usize <= DATA_START {
        return 0;
    }
    let ndata = (total as usize - DATA_START).min(BLK_SIZE * 8);
    let mut free = 0u32;
    for i in 0..ndata {
        if buf[i / 8] & (1 << (i % 8)) == 0 {
            free += 1;
        }
    }
    free
}

/// 数据块总数。
pub fn fs_total_blocks(major: u32) -> u32 {
    let total = total_blocks(major);
    total.saturating_sub(DATA_START as u32)
}

/// 读取 inode 的类型码。
pub fn fs_type(major: u32, ino: u32) -> u16 {
    let mut inode = [0u8; INODE_SIZE];
    if !read_inode(major, ino, &mut inode) {
        return 0;
    }
    inode_type(&inode)
}

/// 读取 inode 的大小（字节）。
pub fn fs_size(major: u32, ino: u32) -> u32 {
    let mut inode = [0u8; INODE_SIZE];
    if !read_inode(major, ino, &mut inode) {
        return 0;
    }
    inode_size(&inode)
}

/// 读取 inode 的设备号（CHR/BLK 用）。
pub fn fs_rdev(major: u32, ino: u32) -> u32 {
    let mut inode = [0u8; INODE_SIZE];
    if !read_inode(major, ino, &mut inode) {
        return 0;
    }
    inode_rdev(&inode)
}
