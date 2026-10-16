//! KunxOS 内核侧 OTA / 安全启动状态管理。
//!
//! - misc 原始区（磁盘尾部 3MB，与 tools/mkdisk.py 一致）：BCB 主/备 + OTP；
//! - 启动成功后写 BCB 成功标记（tries=0、slot_ok=1、构建号提升）；
//! - OTA 安装：KXUP 整包验签（ota_pk）→ 反降级 → 写入非活动槽位 FAT32
//!   （/mnt/fat32 或 /mnt/fat32b）→ 更新 BCB 切换槽位；
//! - 恢复模式：bootargs 含 recovery 时挂载恢复 ramdisk 到 /mnt/recovery。
//!
//! 大缓冲经物理页分配器按需分配（避免把静态大数组揉进内核 .bss）。

use kunxsec::bcb::{Bcb, SLOT_A};
use kunxsec::kxsi;
use kunxsec::kxup::{self, CF_DELTA, CF_FULL};
use kunxsec::otp;
use kunxsec::bspatch;
use kunxsec::keys::{BOOT_PK, OTA_PK};

use crate::allocator;
use crate::blkdev;
use crate::virtio_blk;
use crate::vfs;

/// 磁盘 misc 区几何（与 tools/mkdisk.py 一致）。
pub const MISC_START_LBA: usize = 1_042_432;
pub const BCB_PRIMARY_LBA: usize = MISC_START_LBA;
pub const BCB_BACKUP_LBA: usize = MISC_START_LBA + 1;
pub const OTP_LBA: usize = MISC_START_LBA + 2;
pub const OTP_SECTORS: usize = 8;

/// 挂载路径。
pub const FAT_A_MNT: &str = "/mnt/fat32";
pub const FAT_B_MNT: &str = "/mnt/fat32b";

/// 每镜像文件上限（KXSI blob = 头 80 + 载荷 + 签名 64）。
pub const MAX_KERNEL_BLOB: usize = 0x0201_0000; // 32MB（内核含内嵌 16MB ramdisk）
pub const MAX_RAMDISK_BLOB: usize = 0x0101_0000; // 16MB
pub const MAX_DTB_BLOB: usize = 0x0009_0000;
/// 整包缓冲：最大全量系统包 = 全部组件之和（~40MB）。
pub const MAX_OTA_BLOB: usize = 0x0400_0000; // 64MB

fn send(msg: &str) {
    crate::uart_send_str(msg);
}

fn send_u32(v: u32) {
    crate::print_dec(v as u64);
}

fn send_u8(c: u8) {
    let b = [c];
    crate::uart_send_str(unsafe { core::str::from_utf8_unchecked(&b) });
}

/// 从磁盘父设备（virtio-blk 整盘）读绝对 LBA 扇区。
fn misc_read(lba: usize, sectors: usize, buf: &mut [u8]) -> bool {
    blkdev::blk_read_sectors(virtio_blk::MAJOR, lba, sectors, buf)
        == (sectors * blkdev::SECTOR_SIZE) as i32
}

fn misc_write(lba: usize, sectors: usize, buf: &[u8]) -> bool {
    let ok = blkdev::blk_write_sectors(virtio_blk::MAJOR, lba, sectors, buf)
        == (sectors * blkdev::SECTOR_SIZE) as i32;
    blkdev::blk_flush(virtio_blk::MAJOR) >= 0 && ok
}

/// 读取 BCB（主 + 备，代次取新）。失败返回 None。
pub fn load_bcb() -> Option<Bcb> {
    let mut primary = [0u8; 512];
    let mut backup = [0u8; 512];
    if !misc_read(BCB_PRIMARY_LBA, 1, &mut primary) || !misc_read(BCB_BACKUP_LBA, 1, &mut backup) {
        return None;
    }
    Bcb::parse_double(&primary, &backup)
}

/// 保存 BCB：先备后主。
pub fn save_bcb(b: &Bcb) -> bool {
    let raw = b.serialize();
    misc_write(BCB_BACKUP_LBA, 1, &raw) && misc_write(BCB_PRIMARY_LBA, 1, &raw)
}

/// 读取 OTP。
pub fn load_otp() -> Option<otp::Otp> {
    let mut raw = [0xFFu8; otp::OTP_SIZE];
    // OTP 载荷 2048B = 4 扇区（与 bootloader sec_boot 的 OTP_SECTORS_NEEDED
    // 对齐）。此前误用 8 扇区：blk_read_sectors 要求 buf ≥ count*512=4096，
    // 2048B 缓冲恒失败 → OTP 防回滚楼层内核侧从未生效（R5-M1）。
    const OTP_SECTORS_NEEDED: usize = otp::OTP_SIZE / 512;
    if !misc_read(OTP_LBA, OTP_SECTORS_NEEDED, &mut raw) {
        return None;
    }
    otp::parse(&raw)
}

/// 启动成功：活动槽成功标记 + tries=0 + 构建号提升。
/// `version` = bootloader 注入的构建号（DTB kunx,build-version）。
pub fn note_boot_success(version: u32) {
    if let Some(mut b) = load_bcb() {
        if !b.slot_ok[b.active_slot as usize] {
            crate::kinfo! {
                send("[ota] 首次确认启动槽位 ");
                send_u8(Bcb::slot_name(b.active_slot));
                send("\n");
            }
        }
        b.mark_boot_success(version);
        save_bcb(&b);
        crate::kinfo! {
            send("[ota] 启动成功：槽位 ");
            send_u8(Bcb::slot_name(b.active_slot));
            send(" 构建 ");
            send_u32(b.build_version);
            send("\n");
        }
    } else {
        crate::kinfo! { send("[ota] BCB 不可读，未记录启动成功\n"); }
    }
}

/// 反降级下限：max(BCB 已确认版本, OTP 熔断楼层)。
fn anti_rollback_floor() -> u32 {
    let mut floor = load_bcb().map(|b| b.build_version).unwrap_or(0);
    if let Some(o) = load_otp() {
        let of = o.floor.min(u32::MAX as u64) as u32;
        if of > floor {
            floor = of;
        }
    }
    floor
}

fn component_name(role: u32) -> Option<&'static str> {
    match role {
        kxsi::ROLE_KERNEL => Some("KERNEL.IMG"),
        kxsi::ROLE_DTB => Some("KUNXOS.DTB"),
        kxsi::ROLE_RAMDISK => Some("RAMDISK.IMG"),
        kxsi::ROLE_RECOVERY_RAMDISK => Some("RECOVRD.IMG"),
        _ => None,
    }
}

/// 路径生成：<挂载点>/<文件名>（静态缓冲）。
static mut PATH_BUF: [u8; 64] = [0; 64];

fn path_for(active_is_a: bool, role: u32) -> &'static str {
    let name = component_name(role).unwrap_or("");
    let root = if active_is_a { FAT_B_MNT } else { FAT_A_MNT };
    unsafe {
        let mut n = 0usize;
        for &b in root.as_bytes() {
            PATH_BUF[n] = b;
            n += 1;
        }
        PATH_BUF[n] = b'/';
        n += 1;
        for &b in name.as_bytes() {
            PATH_BUF[n] = b;
            n += 1;
        }
        core::str::from_utf8_unchecked(&PATH_BUF[..n])
    }
}

/// 分页分配一块连续缓冲；失败返回 0。
fn alloc_buf(pages: usize, tag: &str) -> usize {
    match allocator::alloc_pages_contig(pages) {
        Some(b) => b,
        None => {
            send("[ota] 内存不足：");
            send(tag);
            send("\n");
            0
        }
    }
}

fn free_buf(base: usize, pages: usize) {
    if base == 0 {
        return;
    }
    for p in 0..pages {
        allocator::free_page(base + p * allocator::PAGE_SIZE);
    }
}

/// 从路径读文件到物理区 `base`，返回字节数；失败 0。
fn read_file_to(path: &str, base: usize, cap: usize) -> usize {
    let fd = vfs::open(path, vfs::O_RDONLY);
    if fd < 0 {
        return 0;
    }
    let buf = unsafe { core::slice::from_raw_parts_mut(base as *mut u8, cap) };
    let mut total = 0usize;
    loop {
        if total >= cap {
            break;
        }
        let n = vfs::read(fd, &mut buf[total..]);
        if n <= 0 {
            break;
        }
        total += n as usize;
    }
    vfs::close(fd);
    total
}

/// 递归删除 `/data` 下的全部普通文件（保留目录；/data/oobe/.done 被删后
/// OOBE 会重新进入——恢复菜单「清除数据」）。返回删除的文件数。
pub fn wipe_data() -> u32 {
    let mut removed = 0u32;
    wipe_dir("/data", &mut removed);
    send("[ota] 清除：");
    send_u32(removed);
    send(" 个文件已删除，/data 已重置（OOBE 将重新运行）\n");
    removed
}

fn wipe_dir(dir: &str, removed: &mut u32) {
    let mut name = [0u8; 32];
    let mut idx = 0usize;
    loop {
        let ino = vfs::readdir(dir, idx, &mut name);
        if ino < 0 {
            break;
        }
        idx += 1;
        let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        let nm = core::str::from_utf8(&name[..len]).unwrap_or("");
        if nm == "." || nm == ".." {
            continue;
        }
        let mut p = [0u8; 128];
        let mut n = 0usize;
        // 三段写入全程边界检查：dir + '/' + 条目名（≤32B）随递归深度增长，
        // 深层目录会超过 128B —— 此前 p[n] 直接越界 panic（恢复模式崩溃）。
        let mut overflow = false;
        for &b in dir.as_bytes() {
            if n + 1 >= p.len() {
                overflow = true;
                break;
            }
            p[n] = b;
            n += 1;
        }
        if overflow || n + 1 >= p.len() {
            continue;
        }
        p[n] = b'/';
        n += 1;
        for &b in nm.as_bytes() {
            if n + 1 >= p.len() {
                overflow = true;
                break;
            }
            p[n] = b;
            n += 1;
        }
        if overflow {
            continue;
        }
        let pstr = unsafe { core::str::from_utf8_unchecked(&p[..n]) };
        if let Some((typ, _)) = vfs::stat_path(pstr) {
            if typ == crate::fs_simple::T_DIR {
                wipe_dir(pstr, removed);
            } else {
                if vfs::unlink(pstr) == 0 {
                    *removed += 1;
                }
            }
        }
    }
}

/// 把数据（物理区）写入路径（截断重写 + flush）。
fn write_file_from(path: &str, base: usize, len: usize) -> bool {
    let fd = vfs::open(path, vfs::O_WRONLY | vfs::O_CREAT | vfs::O_TRUNC);
    if fd < 0 {
        return false;
    }
    let buf = unsafe { core::slice::from_raw_parts(base as *const u8, len) };
    let mut off = 0usize;
    while off < len {
        let n = vfs::write(fd, &buf[off..]);
        if n <= 0 {
            vfs::close(fd);
            return false;
        }
        off += n as usize;
    }
    vfs::close(fd);
    vfs::fsync(fd) >= 0
}

/// 反降级 + 版本单调检查。
pub fn check_target_version(target: u32) -> bool {
    let floor = anti_rollback_floor();
    if target <= floor {
        send("[ota] 拒绝：目标 ≤ 下限 ");
        send_u32(floor);
        send("\n");
        return false;
    }
    true
}

/// 恢复模式判断：bootargs 含 "recovery"。
pub fn has_recovery_arg(args: &str) -> bool {
    args.split_whitespace().any(|w| w == "recovery")
}

/// 恢复模式：把 bootloader 加载的恢复 ramdisk 内存区包装并挂载到 /mnt/recovery。
pub fn mount_recovery(addr: usize, len: usize) -> i32 {
    if len == 0 || len % blkdev::SECTOR_SIZE != 0 || len > MAX_RAMDISK_BLOB {
        return -1;
    }
    let major = blkdev::ramdisk_wrap("recovrd", 5, addr, len);
    if major < 0 {
        return -1;
    }
    vfs::mount(major as u32, "/mnt/recovery")
}

/// 仅校验模式：读入 KXUP 并整包验签 + 反降级检查（不落盘）。返回 0 通过。
pub fn ota_check(path: &str) -> i32 {
    let pages = MAX_OTA_BLOB.div_ceil(allocator::PAGE_SIZE);
    let pkg_base = alloc_buf(pages, "检查");
    if pkg_base == 0 {
        return -1;
    }
    let total = read_file_to(path, pkg_base, MAX_OTA_BLOB);
    if total < 144 {
        send("[ota] 读取升级包失败\n");
        free_buf(pkg_base, pages);
        return -1;
    }
    let blob = unsafe { core::slice::from_raw_parts(pkg_base as *const u8, total) };
    let pkg = match kxup::parse_and_verify(blob, &OTA_PK, &BOOT_PK) {
        Some(p) => p,
        None => {
            send("[ota] 检查：签名验证失败\n");
            free_buf(pkg_base, pages);
            return -1;
        }
    };
    send("[ota] 检查：通过 target=");
    send_u32(pkg.target_version);
    send(" base=");
    send_u32(pkg.base_version);
    send(" components=");
    send_u32(pkg.comp_count);
    send("\n");
    if !check_target_version(pkg.target_version) {
        free_buf(pkg_base, pages);
        return -1;
    }
    free_buf(pkg_base, pages);
    0
}

/// 设置恢复模式：置 BCB boot_mode=recovery 并落盘。


pub fn ota_set_recovery() -> bool {
    let mut b = match load_bcb() {
        Some(b) => b,
        None => return false,
    };
    b.set_recovery();
    save_bcb(&b)
}

/// 强制切到槽 'A'/'B'（调试/恢复菜单）。


pub fn ota_force_slot(slot: u8) -> bool {
    let mut b = match load_bcb() {
        Some(b) => b,
        None => return false,
    };
    if slot == b'A' {
        b.force_slot(kunxsec::bcb::SLOT_A);
    } else if slot == b'B' {
        b.force_slot(kunxsec::bcb::SLOT_B);
    } else {
        return false;
    }
    save_bcb(&b)
}

/// 打印 OTP / BCB 状态（供 ota status / 恢复菜单）。


pub fn print_status() {
    match load_otp() {
        Some(o) => {
            send("[ota] OTP：secure-boot=");
            send(if o.secure_boot_enabled() { "ON" } else { "off" });
            send(" boot_pk_fused=");
            send(if o.boot_pk_fused() { "YES" } else { "no" });
            send(" device_key=");
            send(if o.device_key_fused() { "fused" } else { "none" });
            send(" floor=");
            send_u32(o.floor.min(u32::MAX as u64) as u32);
            send("\n");
        }
        None => send("[ota] OTP：不可读\n"),
    }
    match load_bcb() {
        Some(b) => {
            send("[ota] BCB：active=");
            send_u8(Bcb::slot_name(b.active_slot));
            send(" tries=");
            send_u32(b.tries_remaining as u32);
            send("/");
            send_u32(b.tries_total as u32);
            send(" ok_A=");
            send(if b.slot_ok[0] { "1" } else { "0" });
            send(" ok_B=");
            send(if b.slot_ok[1] { "1" } else { "0" });
            send(" mode=");
            send(if b.boot_mode == kunxsec::bcb::BOOT_RECOVERY { "recovery" } else { "normal" });
            send(" build=");
            send_u32(b.build_version);
            send("\n");
        }
        None => send("[ota] BCB：不可读\n"),
    }
}

/// 安装 KXUP 更新包到非活动槽并切换 BCB。返回 0 成功。
pub fn ota_install(path: &str) -> i32 {
    let pages = MAX_OTA_BLOB.div_ceil(allocator::PAGE_SIZE);
    let pkg_base = alloc_buf(pages, "升级包");
    if pkg_base == 0 {
        return -1;
    }
    let total = read_file_to(path, pkg_base, MAX_OTA_BLOB);
    if total < 144 {
        send("[ota] 读取升级包失败\n");
        free_buf(pkg_base, pages);
        return -1;
    }
    let blob = unsafe { core::slice::from_raw_parts(pkg_base as *const u8, total) };
    let pkg = match kxup::parse_and_verify(blob, &OTA_PK, &BOOT_PK) {
        Some(p) => p,
        None => {
            send("[ota] 升级包签名验证失败\n");
            free_buf(pkg_base, pages);
            return -1;
        }
    };
    if !check_target_version(pkg.target_version) {
        free_buf(pkg_base, pages);
        return -1;
    }
    let bcb = match load_bcb() {
        Some(b) => b,
        None => {
            send("[ota] BCB 不可读\n");
            free_buf(pkg_base, pages);
            return -1;
        }
    };
    let active_is_a = bcb.active_slot == SLOT_A;
    send("[ota] 安装构建 ");
    send_u32(pkg.target_version);
    send(" 到非活动槽\n");

    for i in 0..pkg.comp_count as usize {
        let c = match kxup::component(&pkg, blob, i) {
            Some(c) => c,
            None => {
                send("[ota] 组件解析失败\n");
                free_buf(pkg_base, pages);
                return -1;
            }
        };
        let base_buf;
        let out_buf;
        let ds_buf;
        let es_buf;
        let final_base: usize;
        let final_len: usize;
        if c.flags & CF_FULL != 0 {
            final_base = pkg_base + c.data_off;
            final_len = c.data_len;
            let fblob = unsafe {
                core::slice::from_raw_parts(final_base as *const u8, final_len)
            };
            if kxsi::verify(fblob, &BOOT_PK, c.role).is_none() {
                send("[ota] 组件 KXSI 校验失败\n");
                free_buf(pkg_base, pages);
                return -1;
            }
            base_buf = 0;
            out_buf = 0;
            ds_buf = 0;
            es_buf = 0;
        } else if c.flags & CF_DELTA != 0 {
            base_buf = alloc_buf(pages, "增量基准");
            out_buf = alloc_buf(pages, "增量输出");
            ds_buf = alloc_buf(pages, "增量差异");
            es_buf = alloc_buf(pages, "增量附加");
            if base_buf == 0 || out_buf == 0 || ds_buf == 0 || es_buf == 0 {
                send("[ota] 增量缓冲内存不足\n");
                free_buf(base_buf, pages);
                free_buf(out_buf, pages);
                free_buf(ds_buf, pages);
                free_buf(es_buf, pages);
                free_buf(pkg_base, pages);
                return -1;
            }
            let base_path = path_for(active_is_a, c.role);
            let base_len = read_file_to(base_path, base_buf, MAX_OTA_BLOB);
            if base_len == 0 {
                send("[ota] 增量基准不可读\n");
                free_buf(base_buf, pages);
                free_buf(out_buf, pages);
                free_buf(ds_buf, pages);
                free_buf(es_buf, pages);
                free_buf(pkg_base, pages);
                return -1;
            }
            let base = unsafe { core::slice::from_raw_parts(base_buf as *const u8, base_len) };
            let patch = unsafe {
                core::slice::from_raw_parts((pkg_base + c.data_off) as *const u8, c.data_len)
            };
            let out = unsafe { core::slice::from_raw_parts_mut(out_buf as *mut u8, MAX_OTA_BLOB) };
            let ds = unsafe { core::slice::from_raw_parts_mut(ds_buf as *mut u8, MAX_OTA_BLOB) };
            let es = unsafe { core::slice::from_raw_parts_mut(es_buf as *mut u8, MAX_OTA_BLOB) };
            match bspatch::apply_patch(base, patch, ds, es, out) {
                None => {
                    send("[ota] 增量应用失败\n");
                    free_buf(base_buf, pages);
                    free_buf(out_buf, pages);
                    free_buf(ds_buf, pages);
                    free_buf(es_buf, pages);
                    free_buf(pkg_base, pages);
                    return -1;
                }
                Some(n) => {
                    let nblob = &out[..n];
                    if kxsi::verify(nblob, &BOOT_PK, c.role).is_none() {
                        send("[ota] 增量结果 KXSI 校验失败\n");
                        free_buf(base_buf, pages);
                        free_buf(out_buf, pages);
                        free_buf(ds_buf, pages);
                        free_buf(es_buf, pages);
                        free_buf(pkg_base, pages);
                        return -1;
                    }
                    final_base = out_buf;
                    final_len = n;
                }
            }
        } else {
            send("[ota] 组件标志无效\n");
            free_buf(pkg_base, pages);
            return -1;
        }
        // 写入非活动槽
        let wpath = path_for(active_is_a, c.role);
        if !write_file_from(wpath, final_base, final_len) {
            send("[ota] 写入非活动槽失败\n");
            if out_buf != 0 {
                free_buf(base_buf, pages);
                free_buf(out_buf, pages);
                free_buf(ds_buf, pages);
                free_buf(es_buf, pages);
            }
            free_buf(pkg_base, pages);
            return -1;
        }
        send("[ota] 组件角色 ");
        send_u32(c.role);
        send(" 已写入\n");
        if out_buf != 0 {
            free_buf(base_buf, pages);
            free_buf(out_buf, pages);
            free_buf(ds_buf, pages);
            free_buf(es_buf, pages);
        }
    }

    // 切换槽位 + OTA 进行中标记
    let mut b = bcb;
    b.prepare_switch();
    if !save_bcb(&b) {
        send("[ota] BCB 切换写入失败\n");
        free_buf(pkg_base, pages);
        return -1;
    }
    send("[ota] 完成：重启以切换槽位\n");
    free_buf(pkg_base, pages);
    0
}