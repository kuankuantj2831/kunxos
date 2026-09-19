// KunxOS 电话/短信子系统自动化自检（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由 kunxinit 在测试模式（/data/modemtest.flag 存在）下拉起。职责：
//   1. 经 /var/run/rild.cmd 下发通话/短信命令
//   2. 经 /dev/modemctl 向内核虚拟调制解调器注入网络侧事件（来电/应答/挂断/短信）
//   3. 轮询 /var/run/rild.state 与 /data/rild/*.dat 断言状态迁移
//   4. 覆盖：拨号→接通→挂断、来电→接听→结束、未接来电、短信发送、
//      短信接收（GSM-7 / UCS-2 / 级联重组）、无 SIM 紧急呼叫
//   5. 全部通过打印 [MODEMTEST] PASS 并以 0 退出；失败打印 FAIL:<步骤> 以 1 退出
//
// 运行：make test-modem（QEMU 无头，宿主脚本断言 PASS）。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::global_asm;
use core::panic::PanicInfo;

// ==================== 系统调用 ====================

const SYS_READ: usize = 63;
const SYS_WRITE: usize = 64;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_NANOSLEEP: usize = 101;
const SYS_GETTIMEOFDAY: usize = 169;
const SYS_EXIT: usize = 93;

const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const O_RDWR: usize = 2;
const O_CREAT: usize = 0x40;

const STATE_PATH: &str = "/var/run/rild.state";
const CMD_PATH: &str = "/var/run/rild.cmd";
const MODEMCTL: &str = "/dev/modemctl";
const INBOX_PATH: &str = "/data/rild/inbox.dat";
const OUTBOX_PATH: &str = "/data/rild/outbox.dat";

// 预生成的 SMS-DELIVER PDU（hex，含 SMSC=00；由 tools/test_pdu.c --deliver 生成）
const PDU_GSM7: &str = "00000B813108000000F100004280722143652010C8329BFD0699E5EF3628C84E8FCB";
const PDU_UCS2: &str = "00000B813108000000F2000842807221436520184F60597DFF0C004B0075006E0078004F0053002077ED4FE1";
const PDU_CONCAT_1: &str = "00100B813108000000F30000428072214365201505000309020182C160B025140A85ADE1703804";
const PDU_CONCAT_2: &str = "00100B813108000000F300004280722143652015050003090202884422B1552C168B2DA3D16804";

#[inline(always)]
unsafe fn syscall1(n: usize, a1: usize) -> usize {
    let ret: usize;
    core::arch::asm!("ecall", in("a7") n, inlateout("a0") a1 => ret, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall2(n: usize, a1: usize, a2: usize) -> usize {
    let ret: usize;
    core::arch::asm!(
        "ecall",
        in("a7") n,
        inlateout("a0") a1 => ret,
        in("a1") a2,
        options(nostack)
    );
    ret
}

#[inline(always)]
unsafe fn syscall3(n: usize, a1: usize, a2: usize, a3: usize) -> usize {
    let ret: usize;
    core::arch::asm!(
        "ecall",
        in("a7") n,
        inlateout("a0") a1 => ret,
        in("a1") a2,
        in("a2") a3,
        options(nostack)
    );
    ret
}

unsafe fn write_fd(fd: usize, buf: &[u8]) -> usize {
    syscall3(SYS_WRITE, fd, buf.as_ptr() as usize, buf.len())
}

fn print(s: &str) {
    unsafe {
        write_fd(1, s.as_bytes());
    }
}

fn print_dec(mut v: u64) {
    if v == 0 {
        print("0");
        return;
    }
    let mut buf = [0u8; 24];
    let mut i = buf.len();
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    unsafe {
        write_fd(1, &buf[i..]);
    }
}

fn sleep_ms(ms: u64) {
    let mut req = [0u8; 16];
    let sec = ms / 1000;
    let nsec = (ms % 1000) * 1_000_000;
    req[..8].copy_from_slice(&sec.to_le_bytes());
    req[8..].copy_from_slice(&nsec.to_le_bytes());
    unsafe {
        syscall2(SYS_NANOSLEEP, req.as_ptr() as usize, 0);
    }
}

fn now_ms() -> u64 {
    let mut tv = [0u8; 16];
    unsafe {
        syscall2(SYS_GETTIMEOFDAY, tv.as_mut_ptr() as usize, 0);
    }
    let sec = u64::from_le_bytes(tv[..8].try_into().unwrap());
    let usec = u64::from_le_bytes(tv[8..].try_into().unwrap());
    sec * 1000 + usec / 1000
}

static mut PATH_BUF: [u8; 96] = [0u8; 96];

unsafe fn open(path: &str, flags: usize) -> isize {
    let n = path.len().min(PATH_BUF.len() - 1);
    PATH_BUF[..n].copy_from_slice(&path.as_bytes()[..n]);
    PATH_BUF[n] = 0;
    syscall3(SYS_OPENAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, flags) as isize
}

/// 追加写文件。
fn write_file(path: &str, data: &[u8]) {
    unsafe {
        let fd = open(path, O_WRONLY | O_CREAT);
        if fd >= 0 {
            syscall3(SYS_WRITE, fd as usize, data.as_ptr() as usize, data.len());
            syscall1(SYS_CLOSE, fd as usize);
        }
    }
}

fn read_file(path: &str, buf: &mut [u8]) -> usize {
    unsafe {
        let fd = open(path, O_RDONLY);
        if fd < 0 {
            return 0;
        }
        // ★ 按 isize 判负并钳制到缓冲上限（wifid 同族 read errno bug）
        let n = syscall3(SYS_READ, fd as usize, buf.as_mut_ptr() as usize, buf.len()) as isize;
        syscall1(SYS_CLOSE, fd as usize);
        if n > 0 {
            (n as usize).min(buf.len())
        } else {
            0
        }
    }
}

fn write_cmd(cmd: &[u8]) {
    write_file(CMD_PATH, cmd);
}

fn write_modemctl(cmd: &[u8]) {
    write_file(MODEMCTL, cmd);
}

// ==================== 状态解析 ====================

#[derive(Clone, Copy, PartialEq)]
enum CallSt {
    Idle,
    Dialing,
    Ringing,
    Active,
    Unknown,
}

struct State {
    call: CallSt,
    dir_out: bool,
    sim_absent: bool,
    missed: bool,
    inbox: u32,
    sms: [u8; 12],
    sms_len: usize,
}

fn read_state() -> State {
    let mut buf = [0u8; 512];
    let n = read_file(STATE_PATH, &mut buf);
    let mut st = State {
        call: CallSt::Unknown,
        dir_out: true,
        sim_absent: false,
        missed: false,
        inbox: 0,
        sms: [0; 12],
        sms_len: 0,
    };
    if n == 0 {
        return st;
    }
    let mut start = 0usize;
    while start < n {
        let end = match buf[start..n].iter().position(|&b| b == b'\n') {
            Some(x) => start + x,
            None => n,
        };
        let line = &buf[start..end];
        if let Some(c) = line.iter().position(|&b| b == b':') {
            let key = &line[..c];
            let val = &line[c + 1..];
            match key {
                b"CALL" => {
                    st.call = match val {
                        b"IDLE" => CallSt::Idle,
                        b"DIALING" => CallSt::Dialing,
                        b"RINGING" => CallSt::Ringing,
                        b"ACTIVE" => CallSt::Active,
                        _ => CallSt::Unknown,
                    };
                }
                b"DIR" => st.dir_out = val == b"OUT",
                b"SIM" => st.sim_absent = val == b"ABSENT",
                b"MISSED" => st.missed = val == b"1",
                b"INBOX" => st.inbox = parse_num(val),
                b"SMS" => {
                    st.sms_len = val.len().min(12);
                    st.sms[..st.sms_len].copy_from_slice(&val[..st.sms_len]);
                }
                _ => {}
            }
        }
        start = end + 1;
    }
    st
}

fn parse_num(s: &[u8]) -> u32 {
    let mut v = 0u32;
    for &b in s {
        if b.is_ascii_digit() {
            v = v * 10 + (b - b'0') as u32;
        }
    }
    v
}

/// 等待状态满足条件，超时返回 false。
fn wait_call(target: CallSt, timeout_ms: u64) -> bool {
    let deadline = now_ms() + timeout_ms;
    loop {
        if read_state().call == target {
            return true;
        }
        if now_ms() >= deadline {
            return false;
        }
        sleep_ms(50);
    }
}

fn wait_sim_absent(timeout_ms: u64) -> bool {
    let deadline = now_ms() + timeout_ms;
    loop {
        if read_state().sim_absent {
            return true;
        }
        if now_ms() >= deadline {
            return false;
        }
        sleep_ms(50);
    }
}

fn wait_sim_ready(timeout_ms: u64) -> bool {
    let deadline = now_ms() + timeout_ms;
    loop {
        if !read_state().sim_absent {
            return true;
        }
        if now_ms() >= deadline {
            return false;
        }
        sleep_ms(50);
    }
}

fn wait_missed(timeout_ms: u64) -> bool {
    let deadline = now_ms() + timeout_ms;
    loop {
        let st = read_state();
        if st.missed && st.call == CallSt::Idle {
            return true;
        }
        if now_ms() >= deadline {
            return false;
        }
        sleep_ms(50);
    }
}

/// 文件内容包含子串。
fn file_contains(path: &str, needle: &[u8]) -> bool {
    let mut buf = [0u8; 8192];
    let n = read_file(path, &mut buf);
    if n == 0 || needle.is_empty() {
        return false;
    }
    buf[..n].windows(needle.len()).any(|w| w == needle)
}

/// 记录数（KXSMS1 格式：行数 - 头）。
fn record_count(path: &str) -> u32 {
    let mut buf = [0u8; 8192];
    let n = read_file(path, &mut buf);
    if n < 8 || !buf[..n].starts_with(b"KXSMS1") {
        return 0;
    }
    let mut count = 0u32;
    for &b in &buf[..n] {
        if b == b'\n' {
            count += 1;
        }
    }
    count.saturating_sub(1)
}

// ==================== 测试 ====================

static mut FAILED: bool = false;

fn step(name: &str, ok: bool) {
    print("[MODEMTEST] ");
    print(name);
    if ok {
        print(": PASS\n");
    } else {
        print(": FAIL\n");
        unsafe { FAILED = true; }
    }
}

fn main_test() {
    // 0. 等待 rild 就绪（状态文件存在且非空）
    let mut ready = false;
    for _ in 0..100 {
        if read_state().call != CallSt::Unknown {
            ready = true;
            break;
        }
        sleep_ms(50);
    }
    step("rild-ready", ready);
    if !ready {
        return;
    }
    sleep_ms(300); // 等 modem 初始化握手完成

    // 1. 主叫：拨号 → 拨号中 → 远端应答 → 通话中 → 挂断
    write_cmd(b"DIAL:13800000001\n");
    step("dial-sent", wait_call(CallSt::Dialing, 5000));
    write_modemctl(b"CALL_ANSWER\n");
    step("remote-answer->active", wait_call(CallSt::Active, 5000));
    write_cmd(b"HANGUP\n");
    step("hangup->idle", wait_call(CallSt::Idle, 5000));

    // 2. 被叫：来电 → 接听 → 通话中 → 远端挂断
    write_modemctl(b"CALL_IN:13800000002\n");
    step("incoming-ringing", wait_call(CallSt::Ringing, 5000));
    {
        let st = read_state();
        step("incoming-dir-IN", !st.dir_out);
    }
    write_cmd(b"ANSWER\n");
    step("answer->active", wait_call(CallSt::Active, 5000));
    write_modemctl(b"CALL_END\n");
    step("remote-end->idle", wait_call(CallSt::Idle, 5000));

    // 3. 未接来电：来电 → 拒绝 → IDLE + MISSED
    write_modemctl(b"CALL_IN:13800000003\n");
    step("missed-ringing", wait_call(CallSt::Ringing, 5000));
    write_cmd(b"HANGUP\n");
    step("missed-flag", wait_missed(5000));

    // 4. 短信发送：SEND → outbox 落盘 + SMS=SENT
    write_cmd("SEND:13800000001\x1FHello from KunxOS\n".as_bytes());
    {
        let mut ok = false;
        for _ in 0..100 {
            let st = read_state();
            if st.sms.starts_with(b"SENT") {
                ok = true;
                break;
            }
            sleep_ms(50);
        }
        step("sms-send-sent", ok);
    }
    step("sms-outbox-persisted", file_contains(OUTBOX_PATH, b"Hello from KunxOS"));

    // 5. 短信接收：GSM-7
    {
        let before = record_count(INBOX_PATH);
        write_file(MODEMCTL, "SMS_IN:".as_bytes());
        write_file(MODEMCTL, PDU_GSM7.as_bytes());
        write_file(MODEMCTL, b"\n");
        let mut ok = false;
        for _ in 0..100 {
            if record_count(INBOX_PATH) > before {
                ok = true;
                break;
            }
            sleep_ms(50);
        }
        step("sms-recv-gsm7-count", ok);
        step("sms-recv-gsm7-text", file_contains(INBOX_PATH, b"Hello from Alice"));
    }

    // 6. 短信接收：UCS-2（中文）
    {
        let before = record_count(INBOX_PATH);
        write_file(MODEMCTL, "SMS_IN:".as_bytes());
        write_file(MODEMCTL, PDU_UCS2.as_bytes());
        write_file(MODEMCTL, b"\n");
        let mut ok = false;
        for _ in 0..100 {
            if record_count(INBOX_PATH) > before {
                ok = true;
                break;
            }
            sleep_ms(50);
        }
        step("sms-recv-ucs2-count", ok);
        step("sms-recv-ucs2-text", file_contains(INBOX_PATH, "你好，KunxOS 短信".as_bytes()));
    }

    // 7. 长短信级联重组：两段到齐后合并为一条
    {
        let before = record_count(INBOX_PATH);
        write_file(MODEMCTL, "SMS_IN:".as_bytes());
        write_file(MODEMCTL, PDU_CONCAT_1.as_bytes());
        write_file(MODEMCTL, b"\n");
        sleep_ms(300);
        write_file(MODEMCTL, "SMS_IN:".as_bytes());
        write_file(MODEMCTL, PDU_CONCAT_2.as_bytes());
        write_file(MODEMCTL, b"\n");
        let mut ok = false;
        for _ in 0..100 {
            if record_count(INBOX_PATH) > before {
                ok = true;
                break;
            }
            sleep_ms(50);
        }
        step("sms-concat-reassembled", ok);
        step("sms-concat-text", file_contains(INBOX_PATH, "AAAA-BBBB-CCCCDDDD-EEEE-FFFF".as_bytes()));
    }

    // 8. 无 SIM 紧急呼叫：SIM:ABSENT 注入 → rild 轮询感知 → 112 可拨，普通号码拒绝
    write_modemctl(b"SIM:ABSENT\n");
    step("sim-absent-detected", wait_sim_absent(6000));
    write_cmd(b"DIAL:112\n");
    step("emergency-dial-allowed", wait_call(CallSt::Dialing, 5000));
    write_cmd(b"HANGUP\n");
    step("emergency-hangup", wait_call(CallSt::Idle, 5000));
    write_cmd(b"DIAL:13800000001\n");
    sleep_ms(1500);
    step("normal-dial-rejected-no-sim", read_state().call == CallSt::Idle);
    write_modemctl(b"SIM:READY\n");
    step("sim-ready-restored", wait_sim_ready(6000));

    print("[MODEMTEST] summary: ");
    if unsafe { FAILED } {
        print("FAILED\n");
    } else {
        print("PASS\n");
    }
}

// ==================== 入口 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[modemtest] starting\n");
    main_test();
    print("[modemtest] done\n");
    unsafe { syscall1(SYS_EXIT, if FAILED { 1 } else { 0 }) };
    0
}

global_asm!(
    ".section .text._start, \"ax\"",
    ".global _start",
    ".type _start, %function",
    "_start:",
    "    mv a1, sp",
    "    ld a0, 0(a1)",
    "    addi a1, a1, 8",
    "    call k_main",
    "    li a7, 93",
    "    ecall",
    "    j .",
);

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // 诊断：把 panic 消息与位置（文件:行:列）写入 fd 2（无堆，固定缓冲）。
    struct BufWriter {
        buf: [u8; 256],
        len: usize,
    }
    impl core::fmt::Write for BufWriter {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let b = s.as_bytes();
            let k = b.len().min(self.buf.len() - self.len);
            self.buf[self.len..self.len + k].copy_from_slice(&b[..k]);
            self.len += k;
            Ok(())
        }
    }
    let mut w = BufWriter { buf: [0u8; 256], len: 0 };
    let _ = core::fmt::Write::write_fmt(&mut w, core::format_args!("{}\n", info));
    unsafe {
        write_fd(2, b"PANIC: modemtest\n");
        write_fd(2, &w.buf[..w.len]);
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
