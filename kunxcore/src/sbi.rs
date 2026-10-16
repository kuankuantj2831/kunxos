//! SBI（Supervisor Binary Interface）调用封装。
//!
//! 经 OpenSBI（QEMU `-bios default` 的 fw_dynamic）提供 S 模式不可直达的
//! 服务：定时器编程（TIME 扩展）、系统关机/复位（SRST 扩展，回退 sifive
//! test 器件直写）。调用遵循 SBI v0.2+ 约定：`a6`=FID、`a7`=EID，返回
//! `a0`=错误码、`a1`=值。
//!
//! 与 aarch64 版本（PSCI `smc/hvc`）的对应关系：
//!   PSCI SYSTEM_OFF   → sbi_srst_shutdown / test 器件 0x5555
//!   PSCI SYSTEM_RESET → sbi_srst_reset    / test 器件 0x7777
//!   CNTV/CNTP 定时器  → TIME 扩展 set_timer（单硬件通道，timer.rs 多路复用）

/// SBI 错误码：成功。
pub const SBI_SUCCESS: i64 = 0;
/// SBI 错误码：不支持。
pub const SBI_ERR_NOT_SUPPORTED: i64 = -2;

// SBI 扩展 ID
const EID_BASE: u64 = 0x10;
const EID_TIME: u64 = 0x5449_4D45; // "TIME"
const EID_SRST: u64 = 0x5352_5354; // "SRST"
/// 旧式（SBI v0.1）shutdown：EID 0x08。OpenSBI 仍实现，作 SRST 回退。
const EID_LEGACY_SHUTDOWN: u64 = 0x08;

/// SBI v0.2+ ecall：返回 (错误码, 值)。
///
/// 参数按约定进 a0/a1/a2（此前 a1/a2 参数误入 a2/a3 寄存器——单参数
/// 调用（set_timer）不受影响，首个双参数调用 hart_start 把入口地址
/// 传成了垃圾，次核跳转到随机地址静默死亡）。
#[inline]
fn sbi_call(eid: u64, fid: u64, a0: u64, a1: u64, a2: u64) -> (i64, i64) {
    let err: i64;
    let val: i64;
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("a0") a0 as i64 => err,
            inlateout("a1") a1 as i64 => val,
            in("a2") a2 as i64,
            in("a6") fid as i64,
            in("a7") eid as i64,
            options(nostack)
        );
    }
    (err, val)
}

/// 探测扩展是否可用（BASE 扩展 fid 3 = probe_extension）。
fn probe_extension(eid: u64) -> bool {
    let (err, val) = sbi_call(EID_BASE, 3, eid, 0, 0);
    err == SBI_SUCCESS && val != 0
}

/// SRST 扩展可用性（惰性探测缓存；`u8::MAX` = 未探测）。
static mut SRST_OK: u8 = u8::MAX;

fn srst_available() -> bool {
    unsafe {
        if SRST_OK == u8::MAX {
            SRST_OK = probe_extension(EID_SRST) as u8;
        }
        SRST_OK != 0
    }
}

/// HSM 扩展 EID（"HSM"）。
const EID_HSM: u64 = 0x4853_4D;

/// 启动指定 hart：使其从 `start_addr`（a0=hartid, a1=opaque）开始执行。
///
/// OpenSBI 只把 **boot hart** 带到内核入口，其余核停在 OpenSBI 的 HSM
/// 停车场——内核必须主动 hart_start 它们（此前假设「所有 hart 都到达
/// _start」，次核从未上线，SMP 实际不可用：互监看门狗/负载均衡全部空转）。
/// 返回 0 成功；-1 表示已在运行/不存在等（幂等，忽略即可）。
pub fn hart_start(hartid: u64, start_addr: u64, opaque: u64) -> i64 {
    let (err, _val) = sbi_call(EID_HSM, 0, hartid, start_addr, opaque);
    err
}

/// 设置下一个监督定时器到期时刻（绝对 time 值，TIME 扩展 fid 0）。
///
/// 到期触发监督定时器中断（scause=5）；S 模式无直接清除口，重装即应答——
/// 这正是 timer.rs `timer_ack`/`reload` 的语义。
pub fn set_timer(stime: u64) {
    let _ = sbi_call(EID_TIME, 0, stime, 0, 0);
}

/// 关机：SRST（shutdown 类型）→ 旧式 shutdown → sifive test 器件（0x5555）。
pub fn shutdown() -> ! {
    if srst_available() {
        // type=0（shutdown）、reason=0（无原因）
        let _ = sbi_call(EID_SRST, 0, 0, 0, 0);
    }
    let (_, _) = sbi_call(EID_LEGACY_SHUTDOWN, 0, 0, 0, 0);
    // 最后回退：QEMU virt sifive test 器件（0x100000），写 0x5555 = 退出
    unsafe {
        core::ptr::write_volatile(0x0010_0000usize as *mut u32, 0x5555);
    }
    loop {
        core::hint::spin_loop();
    }
}

/// 重启：SRST（cold reboot）→ sifive test 器件（0x7777）。
pub fn system_reset() -> ! {
    if srst_available() {
        // type=1（cold reboot）、reason=0
        let _ = sbi_call(EID_SRST, 0, 1, 0, 0);
    }
    unsafe {
        core::ptr::write_volatile(0x0010_0000usize as *mut u32, 0x7777);
    }
    loop {
        core::hint::spin_loop();
    }
}
