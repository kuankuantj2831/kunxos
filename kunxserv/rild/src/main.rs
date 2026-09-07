// KunxOS rild：RIL 守护进程（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 职责：
//   1. 打开 /dev/modem（内核 Modem HAL：QEMU 虚拟调制解调器 / Pi4 UART2）
//   2. AT 命令层：初始化握手（SIM/信号/运营商/注册/短信中心）、URC 派发
//   3. RIL 状态机：通话（dial/hangup/answer + CLCC 轮询）、短信收发
//      （SMS-SUBMIT/DELIVER PDU，C 库 libpdu 编解码）、紧急呼叫
//   4. IPC：应用写 /var/run/rild.cmd 下发命令；rild 持续写 /var/run/rild.state
//   5. 持久化：/data/rild/inbox.dat + outbox.dat（短信）、/data/contacts.dat（通讯录）
//   6. 音频联动：来电铃声、通话焦点、路由（听筒/扬声器，经 /dev/audio）
//
// 启动：kunxinit fork + execve 拉起（/bin/rild），常驻运行。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::global_asm;
use core::panic::PanicInfo;

mod at;
mod pdu;
mod ril;
mod sys;

pub use ril::log;
use sys::*;

// ==================== 命令管道 ====================

const CMD_PATH: &str = "/var/run/rild.cmd";
const DATA_DIR: &str = "/data/rild";

/// 处理 /var/run/rild.cmd 中的一行命令。
fn handle_command(cmd: &[u8]) {
    let t = cmd.trim_ascii();
    if t.is_empty() {
        return;
    }
    if let Some(num) = strip_prefix(t, b"DIAL:") {
        ril::dial(num);
    } else if t == b"HANGUP" {
        ril::hangup();
    } else if t == b"ANSWER" {
        ril::answer();
    } else if let Some(rest) = strip_prefix(t, b"SEND:") {
        ril::send_sms(rest);
    } else if let Some(num) = strip_prefix(t, b"SET_SMSC:") {
        ril::set_smsc(num);
    } else if let Some(id) = strip_prefix(t, b"SMS_DEL:") {
        if let Some(n) = at::parse_num(id) {
            ril::delete_sms(n);
        }
    } else if let Some(id) = strip_prefix(t, b"SMS_READ:") {
        if let Some(n) = at::parse_num(id) {
            ril::mark_read(n);
        }
    } else if t == b"QUERY" {
        ril::mark_dirty();
    } else {
        print("[rild] 未知命令\n");
    }
}

fn strip_prefix<'a>(s: &'a [u8], p: &[u8]) -> Option<&'a [u8]> {
    if s.starts_with(p) {
        Some(&s[p.len()..])
    } else {
        None
    }
}

trait TrimAsciiExt {
    fn trim_ascii(&self) -> &[u8];
}
impl TrimAsciiExt for [u8] {
    fn trim_ascii(&self) -> &[u8] {
        let mut s = 0;
        let mut e = self.len();
        while s < e && (self[s] == b' ' || self[s] == b'\t' || self[s] == b'\r' || self[s] == b'\n') {
            s += 1;
        }
        while e > s && (self[e - 1] == b' ' || self[e - 1] == b'\t' || self[e - 1] == b'\r' || self[e - 1] == b'\n') {
            e -= 1;
        }
        &self[s..e]
    }
}

/// 读取并清空命令管道（普通文件模拟 FIFO）。
fn poll_commands() {
    static mut BUF: [u8; 1024] = [0u8; 1024];
    unsafe {
        let fd = open(CMD_PATH, O_RDWR);
        if fd < 0 {
            return;
        }
        syscall3(SYS_LSEEK, fd as usize, 0, SEEK_SET);
        let n = syscall3(SYS_READ, fd as usize, BUF.as_mut_ptr() as usize, BUF.len());
        // 清空管道文件（ftruncate 显式传 0）
        syscall2(SYS_FTRUNCATE, fd as usize, 0);
        syscall3(SYS_LSEEK, fd as usize, 0, SEEK_SET);
        if n > 0 && (n as usize) <= BUF.len() {
            let data = &BUF[..n as usize];
            let mut start = 0usize;
            for i in 0..=data.len() {
                if i == data.len() || data[i] == b'\n' {
                    if i > start {
                        handle_command(&data[start..i]);
                    }
                    start = i + 1;
                }
            }
        }
        syscall1(SYS_CLOSE, fd as usize);
    }
}

// ==================== 主循环 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[rild] KunxOS RIL 守护进程正在启动\n");

    unsafe {
        // 数据目录
        let _ = mkdir(DATA_DIR);

        // 打开调制解调器与音频设备
        let at = &mut *core::ptr::addr_of_mut!(ril::AT);
        at.open_modem();
        if at.fd < 0 {
            print("[rild] FATAL: 无法打开 /dev/modem err=");
            print_dec((-at.fd) as u64);
            print("\n");
        }
        // 诊断探测（原 /dev/null、/tmp、/、/data/oobe/theme 打开性检查已移除：
        // 每次开机固定刷 6 行无用日志；文件系统异常时打开路径会自然报错）。
        ril::AUDIO_FD = open("/dev/audio", O_RDWR);
        if ril::AUDIO_FD < 0 {
            print("[rild] /dev/audio 不可用 (提示音已禁用)\n");
        }

        // 创建命令管道文件
        let fd = open(CMD_PATH, O_CREAT | O_RDWR);
        if fd >= 0 {
            syscall1(SYS_CLOSE, fd as usize);
        } else {
            print("[rild] FATAL: 无法创建 ");
            print(CMD_PATH);
            print("\n");
        }

        // 通讯录缓存
        ril::load_contacts();

        // Modem 初始化握手
        if at.fd >= 0 {
            ril::init_modem();
        } else {
            // 无调制解调器：SIM=ABSENT，仍可紧急呼叫（经 modemctl 注入的模拟）
            ril::RIL.sim = ril::SimState::Absent;
            ril::RIL.modem_up = true;
            ril::mark_dirty();
        }
    }

    let mut tick = 0u64;
    loop {
        unsafe {
            // 1. 调制解调器 URC
            let at = &mut *core::ptr::addr_of_mut!(ril::AT);
            at.poll_urc();

            // 2. 通话状态轮询（CLCC）
            ril::poll_clcc();

            // 3. 应用命令
            poll_commands();

            // 4. 周期任务（状态文件 / 联系人缓存）
            ril::periodic();

            tick += 1;
            if tick % 40 == 0 && ril::RIL.modem_up {
                // 每 ~2 秒日志心跳（调试辅助）
            }
        }
        sleep_ms(50);
    }
}

// ==================== 入口 ====================

global_asm!(
    ".section .text._start, \"ax\"",
    ".global _start",
    ".type _start, %function",
    "_start:",
    "    mov x2, sp",
    "    ldr x0, [x2], #8",
    "    mov x1, x2",
    "    bl k_main",
    "    mov x8, #93",
    "    svc #0",
    "    b .",
);

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // 诊断：打印 panic 位置（文件:行:列），无堆、固定缓冲。
    struct PanicBuf { buf: [u8; 256], len: usize }
    impl core::fmt::Write for PanicBuf {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let b = s.as_bytes();
            let k = b.len().min(self.buf.len() - self.len);
            self.buf[self.len..self.len + k].copy_from_slice(&b[..k]);
            self.len += k;
            Ok(())
        }
    }
    let mut w = PanicBuf { buf: [0u8; 256], len: 0 };
    let _ = core::fmt::Write::write_fmt(&mut w, core::format_args!("{}\n", info));
    unsafe {
        write_fd(2, b"PANIC: rild\n");
        if w.len > 0 {
            write_fd(2, &w.buf[..w.len]);
        }
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
