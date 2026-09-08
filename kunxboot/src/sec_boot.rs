//! 安全启动流程（kunxboot）：BCB 槽位决策 + OTP 信任锚 + KXSI 验签 + 恢复模式。
//!
//! 流程（对应 docs/secure-boot.md §5.1）：
//! 1. 读 misc 区 BCB 主/备 → 槽位决策（recovery / normal / 自动回滚）；
//! 2. 读 OTP：secure-boot 使能熔丝、boot_pk 摘要（BootROM 之后的自研信任根）；
//!    若 OTP 未烧录 → 按出厂流程自动烧录（首次启动 provisioning）；
//! 3. 挂载目标槽位的 FAT32 卷，加载 KERNEL.IMG / KUNXOS.DTB / RAMDISK.IMG
//!    （或恢复模式加载 RECOVRD.IMG），逐镜像 KXSI 验签（先校验 boot_pk 摘要）；
//! 4. 验签失败 → 尝试另一槽位；两槽都失败 → 置恢复模式并停留；
//! 5. 成功 → 把载荷拷贝到约定地址，返回 BootPlan（入口 / 镜像长度 / 构建号）。

use kunxsec::bcb::{Bcb, BOOT_RECOVERY};
use kunxsec::kxsi::{self, Kxsi};
use kunxsec::otp::{self, Otp};
use kunxsec::keys::{BOOT_PK, BOOT_PK_SHA256};

use crate::fs_fat::FatVolume;
use crate::mmc;
use crate::load_kernel::{
    BCB_BACKUP_LBA, BCB_PRIMARY_LBA, DTB_LOAD_ADDR, KERNEL_LOAD_ADDR, OTP_LBA, OTP_SECTORS,
    RAMDISK_LOAD_ADDR, RECOVERY_RAMDISK_LOAD_ADDR,
};
use crate::uart;

/// 每镜像的文件上限：KXSI blob = 头 80 + 载荷 + 签名 64，故比载荷上限略大。
/// KERNEL 上限受布局约束：0x40080000 + 上限 ≤ DTB_LOAD_ADDR(0x44000000)，
/// 即最大 0x3F80000（63.5MB）。实际内核已含内嵌 16MB ramdisk 约 58MB，
/// 旧的 48MB 上限会把合法内核拒之门外（双槽全拒 → 误入 recovery）。
pub const MAX_KERNEL_FILE: usize = 0x03F8_0000; // 63.5MB（紧贴 DTB 区下界）
// 必须与 dtb::SCRATCH_SIZE 一致：fixup 的 scratch 只有 128KB，超过即
// 「DTB 修正失败」→ hang() —— 载荷上限 576KB 时 128KB~576KB 的合法 DTB
// 会把启动锁死。编译期断言见下。
pub const MAX_DTB_FILE: usize = 0x0002_0000;
pub const MAX_RAMDISK_FILE: usize = 0x0101_0000; // 16MB 载荷 + …
pub const MAX_RECOVRD_FILE: usize = 0x0101_0000; // 16MB 载荷 + …

/// RAM 末尾（QEMU virt 128MB，与 main.rs 的 RAM_SIZE 一致）。
/// 编译期校验各加载区上限不越出 RAM：上限或地址调整后若越界，直接编译失败，
/// 而非运行期 DMA 写进未映射地址（静默损坏/总线错误）。
const RAM_END: usize = 0x4800_0000;
const _: () = assert!(
    KERNEL_LOAD_ADDR + MAX_KERNEL_FILE <= RAM_END,
    "KERNEL 加载区越出 RAM 末尾"
);
const _: () = assert!(
    DTB_LOAD_ADDR + MAX_DTB_FILE <= RAM_END,
    "DTB 加载区越出 RAM 末尾"
);
const _: () = assert!(
    RAMDISK_LOAD_ADDR + MAX_RAMDISK_FILE <= RAM_END,
    "RAMDISK 加载区越出 RAM 末尾"
);
const _: () = assert!(
    RECOVERY_RAMDISK_LOAD_ADDR + MAX_RECOVRD_FILE <= RAM_END,
    "RECOVERY RAMDISK 加载区越出 RAM 末尾"
);

/// 启动种类。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BootKind {
    Normal,
    Recovery,
}

/// 一次安全启动的成果：入口、各镜像载荷长度、使用的槽位、构建号。
pub struct BootPlan {
    pub kind: BootKind,
    pub slot: u8,
    pub entry: u64,
    pub kernel_len: usize,
    pub dtb_len: usize,
    pub ramdisk_len: usize,
    pub recovrd_len: usize,
    pub build: u32,
}

static mut OTP_BUF: [u8; otp::OTP_SIZE] = [0xFF; otp::OTP_SIZE];

/// OTP 载荷实际占用的扇区数：otp::OTP_SIZE=2048B = 4 扇区（原始区虽划 8 扇区，
/// 载荷只落前 4 扇区；读写一律按此数，杜绝 2048B 缓冲被 8 扇区读写越界）。
const OTP_SECTORS_NEEDED: usize = if OTP_SECTORS < otp::OTP_SIZE / 512 {
    OTP_SECTORS
} else {
    otp::OTP_SIZE / 512
};

fn sec(msg: &str) {
    uart::send_str("[sec] ");
    uart::send_str(msg);
    uart::send_str("\n");
}

/// 读取 OTP 区（实际载荷 2048B = 4 扇区，读到 2KB 静态缓冲；任一扇区失败
/// 返回 None —— 不得把失败伪装成“全新（全 0xFF）”，否则会触发破坏性重烧录）。
fn read_otp() -> Option<[u8; otp::OTP_SIZE]> {
    unsafe {
        let dst = OTP_BUF.as_mut_ptr();
        for i in 0..OTP_SECTORS_NEEDED {
            if !mmc::read_block(OTP_LBA + i as u64, dst.add(i * 512)) {
                return None;
            }
        }
        Some(OTP_BUF)
    }
}

/// 写回 OTP 区（provisioning 后落盘；只写 OTP_SIZE 对应的扇区数）。
fn write_otp(raw: &[u8; otp::OTP_SIZE]) -> bool {
    unsafe {
        let src = raw.as_ptr();
        for i in 0..OTP_SECTORS_NEEDED {
            if !mmc::write_block(OTP_LBA + i as u64, src.add(i * 512)) {
                return false;
            }
        }
    }
    true
}

/// 读取 BCB（主 + 备，代次取新，CRC 校验；无效 → 全新）。
fn read_bcb() -> Bcb {
    let mut primary = [0u8; 512];
    let mut backup = [0u8; 512];
    if !mmc::read_block(BCB_PRIMARY_LBA, primary.as_mut_ptr()) {
        return Bcb::new();
    }
    if !mmc::read_block(BCB_BACKUP_LBA, backup.as_mut_ptr()) {
        return Bcb::new();
    }
    match Bcb::parse_double(&primary, &backup) {
        Some(b) => b,
        None => Bcb::new(),
    }
}

/// 保存 BCB：先写备、再写主（崩溃时主/备至少一版有效）；两次写入结果都检查。
fn write_bcb(b: &Bcb) -> bool {
    let raw = b.serialize();
    let ok = mmc::write_block(BCB_BACKUP_LBA, raw.as_ptr());
    ok && mmc::write_block(BCB_PRIMARY_LBA, raw.as_ptr())
}

/// 校验 boot 公钥与 OTP 熔断摘要一致（篡改镜像连内嵌公钥一起换也防不住）。
fn boot_pk_matches_otp(o: &Otp) -> bool {
    if !o.boot_pk_fused() {
        return true; // 未熔断时放行（provisioning 前），首次启动会熔断
    }
    o.boot_pk_matches(&BOOT_PK_SHA256)
}

/// 校验镜像授权：OTP secure-boot 熔断后强制要求 boot_pk 摘要匹配。
fn authorize(o: &Otp) -> bool {
    if !o.secure_boot_enabled() {
        return true; // 未置 secure-boot 熔丝 → 引导模式保持宽松（首启 provisioning）
    }
    boot_pk_matches_otp(o)
}

/// 首次启动 provisioning：OTP 未烧录时按出厂流程熔断 secure-boot + boot_pk 摘要，
/// 并落盘。幂等：已烧录则不动。设备唯一密钥需真随机源，此处不生成（0xFF=未生成）。
fn provision_otp_if_needed() -> bool {
    let raw = match read_otp() {
        Some(r) => r,
        None => {
            sec("OTP 读取失败（中止预置）");
            return false;
        }
    };
    match otp::parse(&raw) {
        Some(_) => true, // 已烧录：幂等返回成功
        None => {
            sec("OTP：全新，预置 secure-boot + boot_pk + 设备密钥");
            let mut r = otp::fresh();
            otp::burn_flags(&mut r, otp::FLAG_SECURE_BOOT);
            otp::burn_boot_pk(&mut r, &BOOT_PK_SHA256);
            // 设备唯一密钥：bootloader 阶段无真随机源（无 rng 驱动，栈地址 +
            // 计数器等软件熵在裸机上完全可预测、可离线重算）。信任根材料绝不
            // 允许烧入可预测值——宁可不生成（OTP 语义 0xFF = 未生成，见
            // kunxsec::otp），留给出厂烧录阶段注入真随机（见
            // docs/secure-boot.md §7.1），需要时由内核 virtio-rng 补做。
            sec("警告：无真随机源，跳过设备密钥烧录（0xFF=未生成）");
            write_otp(&r)
        }
    }
}

/// 验证一个 KXSI 镜像（先授权检查再验签）。失败打印原因。
fn verify_image(blob: &[u8], role: u32, o: &Otp) -> Option<Kxsi> {
    if !authorize(o) {
        sec("验签中止：boot_pk 摘要不匹配（OTP 锚点）");
        return None;
    }
    let v = kxsi::verify(blob, &BOOT_PK, role)?;
    Some(v)
}

/// 从 `vol` 加载并验证单个文件，把载荷原地落位到 `load_addr`。
/// 返回 (载荷长度, 头信息)。
fn load_one(
    vol: &FatVolume,
    name: &str,
    role: u32,
    load_addr: usize,
    capacity: usize,
    o: &Otp,
) -> Option<(usize, Kxsi)> {
    let e = vol.find(name)?;
    if e.size as usize > capacity {
        sec("验签失败：文件过大");
        return None;
    }
    // 先整体读入 load_addr（staging），再验证
    if !vol.read_file(&e, load_addr as *mut u8, capacity) {
        sec("验签失败：读取错误，加载 ");
        sec(name);
        sec("");
        return None;
    }
    let blob =
        unsafe { core::slice::from_raw_parts(load_addr as *const u8, e.size as usize) };
    let v = verify_image(blob, role, o)?;
    // 载荷落位：blob[payload_off .. payload_off+len] → load_addr
    let off = v.payload_off;
    let len = v.payload_len;
    unsafe {
        let src = blob.as_ptr().add(off);
        let dst = load_addr as *mut u8;
        // 向前拷贝（src 高于 dst，安全）
        for i in 0..len {
            *dst.add(i) = *src.add(i);
        }
    }
    Some((len, v))
}

/// 槽位 → MBR 分区索引：A=0（boot_a，P1），B=3（boot_b，P4）。
fn slot_partition_index(slot: u8) -> usize {
    if slot == kunxsec::bcb::SLOT_B {
        3
    } else {
        0
    }
}

/// 尝试从第 `slot` 个 boot 分区加载全部镜像；普通 / 恢复模式产物不同。
fn try_load_slot(slot: u8, kind: BootKind, o: &Otp) -> Option<BootPlan> {
    let vol = FatVolume::mount_index(slot_partition_index(slot))?;
    let (klen, kv) = load_one(
        &vol,
        "KERNEL.IMG",
        kxsi::ROLE_KERNEL,
        KERNEL_LOAD_ADDR,
        MAX_KERNEL_FILE,
        o,
    )?;
    let (dlen, _dv) = load_one(
        &vol,
        "KUNXOS.DTB",
        kxsi::ROLE_DTB,
        DTB_LOAD_ADDR,
        MAX_DTB_FILE,
        o,
    )?;
    let mut ram = 0;
    let mut rec = 0;
    match kind {
        BootKind::Normal => {
            let (l, _rv) = load_one(
                &vol,
                "RAMDISK.IMG",
                kxsi::ROLE_RAMDISK,
                RAMDISK_LOAD_ADDR,
                MAX_RAMDISK_FILE,
                o,
            )?;
            ram = l;
        }
        BootKind::Recovery => {
            let (l, _rv) = load_one(
                &vol,
                "RECOVRD.IMG",
                kxsi::ROLE_RECOVERY_RAMDISK,
                RECOVERY_RAMDISK_LOAD_ADDR,
                MAX_RECOVRD_FILE,
                o,
            )?;
            rec = l;
        }
    }
    Some(BootPlan {
        kind,
        slot,
        entry: kv.resolved_entry(),
        kernel_len: klen,
        dtb_len: dlen,
        ramdisk_len: ram,
        recovrd_len: rec,
        build: kv.version,
    })
}

/// 槽位决策（normal 模式）。返回 (候选槽, 是否需写回 BCB 的尝试计数递减)。
fn elect_normal(bcb: &Bcb) -> (u8, bool) {
    let slot = bcb.active_slot;
    let confirmed = bcb.slot_ok[slot as usize];
    // 出厂首启（gen==0，无任何切换历史）：直接启动活动槽，不消耗尝试、不判回滚。
    if bcb.gen == 0 {
        return (slot, false);
    }
    let mut tries = bcb.tries_remaining;
    let mut changed = false;
    if !confirmed && tries > 0 {
        tries -= 1;
        changed = true;
    }
    if tries == 0 && !confirmed {
        // 尝试耗尽且从未成功 → 回滚到另一槽
        let other = Bcb::other_slot(slot);
        sec("回滚：A/B 尝试已耗尽，切换槽位");
        (other, changed)
    } else {
        (slot, changed)
    }
}

/// 主入口：读 BCB + OTP，决策槽位，加载并验签，写回 BCB 状态。
/// 返回 BootPlan；失败（两槽均不可信）返回 None（main 将停留告警）。
pub fn secure_boot_flow() -> Option<BootPlan> {
    // 首次读取仅用于确认 OTP 可读：任一扇区失败即中止启动，避免把失败
    // 误判为“全新 OTP”而触发破坏性重复烧录。
    if read_otp().is_none() {
        sec("OTP 读取失败");
        return None;
    }
    if !provision_otp_if_needed() {
        sec("OTP：预置失败");
    }
    // 重新读取（烧录后）
    let o = match read_otp() {
        None => {
            sec("预置后 OTP 读取失败");
            return None;
        }
        Some(raw) => match otp::parse(&raw) {
            Some(o) => o,
            None => {
                sec("OTP：回读后无效");
                return None;
            }
        },
    };
    if o.secure_boot_enabled() {
        sec("OTP：secure-boot 已熔断，信任锚已锁定");
    } else {
        sec("OTP：安全启动未使能（待预置）");
    }

    let mut bcb = read_bcb();
    sec("BCB：槽位 A/B 状态已加载");

    // 恢复模式：从活动槽（失败则另一槽）的 boot 分区加载恢复镜像。
    // 恢复模式不会在此清除——由恢复菜单/内核显式改回 normal。
    if bcb.boot_mode == BOOT_RECOVERY {
        sec("启动模式：恢复");
        if let Some(plan) = try_load_slot(bcb.active_slot, BootKind::Recovery, &o) {
            return Some(plan);
        }
        let other = Bcb::other_slot(bcb.active_slot);
        sec("恢复槽失败，尝试另一槽位");
        if let Some(plan) = try_load_slot(other, BootKind::Recovery, &o) {
            return Some(plan);
        }
        sec("恢复失败：无受信任的恢复镜像");
        return None;
    }

    // 普通模式：槽位决策（尝试计数 / 自动回滚）
    let (slot, decrement) = elect_normal(&bcb);
    let mut used_other = false;
    let plan = match try_load_slot(slot, BootKind::Normal, &o) {
        Some(plan) => plan,
        None => {
            sec("验签失败：主候选被拒绝");
            let other = Bcb::other_slot(slot);
            match try_load_slot(other, BootKind::Normal, &o) {
                Some(plan) => {
                    used_other = true;
                    plan
                }
                None => {
                    sec("验签失败：两槽皆被拒绝 → 进入恢复");
                    let mut r = bcb;
                    r.set_recovery();
                    write_bcb(&r);
                    return None;
                }
            }
        }
    };

    // 写回 BCB（保持其它字段）：
    // - 用另一槽启动（回滚/验签失败切换）→ 切换 active 并换代；
    // - 尝试计数递减（未确认的槽每次启动消耗一次机会）。
    if used_other {
        bcb.active_slot = Bcb::other_slot(bcb.active_slot);
        bcb.gen = bcb.gen.wrapping_add(1);
    }
    if decrement {
        bcb.tries_remaining = if bcb.tries_remaining > 0 { bcb.tries_remaining - 1 } else { 0 };
        bcb.gen = bcb.gen.wrapping_add(1);
    }
    write_bcb(&bcb);

    Some(plan)
}