// KunxOS 短信 modem 接口：与 rild（提示词 #14 的 RIL 服务）对接。
//
// 发送流程：检查 /var/run/rild.state 的 SIM 状态，无 SIM 时消息存为草稿；
// 有 SIM 时写入 SEND 命令并标记「发送中」，随后轮询 SMS 状态字段推进为
// 「已发送 / 失败」。
//
// 接收流程：轮询 rild.state 的 INBOX 计数变化（RIL 推送），变化时把
// /data/rild/inbox.dat 中水位之后的新记录导入 /data/sms/ 数据库，并返回
// changed 由调用方重绘 UI（通知 UI）。

use crate::kit;
use crate::store;

const STATE_PATH: &[u8] = b"/var/run/rild.state";
const CMD_PATH: &[u8] = b"/var/run/rild.cmd";
const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const O_CREAT: usize = 0x40;
const SYS_READ: usize = 63;
const SYS_CLOSE: usize = 57;
const SYS_OPENAT: usize = 56;
const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;

static mut SIM_READY: bool = false;
static mut LAST_INBOX: u32 = 0;
static mut SMS_STATE: [u8; 16] = [0; 16];
static mut SMS_STATE_LEN: usize = 0;
static mut SAW_SENDING: bool = false;

static mut PATH_BUF: [u8; 48] = [0; 48];

fn open_bytes(path: &[u8], flags: usize) -> isize {
    unsafe {
        let n = path.len().min(PATH_BUF.len() - 1);
        PATH_BUF[..n].copy_from_slice(path);
        PATH_BUF[n] = 0;
        kit::syscall3(SYS_OPENAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, flags) as isize
    }
}

fn read_state(buf: &mut [u8]) -> usize {
    unsafe {
        let fd = open_bytes(STATE_PATH, O_RDONLY);
        if fd < 0 {
            return 0;
        }
        let n = kit::syscall3(SYS_READ, fd as usize, buf.as_mut_ptr() as usize, buf.len());
        kit::syscall1(SYS_CLOSE, fd as usize);
        // read 失败返回 -errno（usize 巨大值）：按 isize 判负并钳制到缓冲，
        // 否则 poll() 里 buf[start..n] 越界 panic。
        let n = n as isize;
        if n <= 0 {
            0
        } else {
            (n as usize).min(buf.len())
        }
    }
}

fn write_cmd(cmd: &[u8]) {
    unsafe {
        let fd = open_bytes(CMD_PATH, O_WRONLY | O_CREAT);
        if fd >= 0 {
            kit::write_fd(fd as usize, cmd);
            kit::syscall1(SYS_CLOSE, fd as usize);
        }
    }
}

fn parse_u32(s: &[u8]) -> Option<u32> {
    if s.is_empty() {
        return None;
    }
    let mut v = 0u32;
    for &b in s {
        if !b.is_ascii_digit() {
            return None;
        }
        v = v.wrapping_mul(10).wrapping_add((b - b'0') as u32);
    }
    Some(v)
}

/// 当前 SIM 是否就绪（由最近一次 poll 缓存）。
pub fn sim_ready() -> bool {
    unsafe { SIM_READY }
}

/// 发送短信：无 SIM → 存草稿；有 SIM → 写 SEND 命令并标记发送中。
/// 返回消息实际状态。
pub fn send(number: &[u8], text: &[u8]) -> store::SmsStatus {
    let mut stamp = [0u8; 16];
    let sl = store::now_stamp(&mut stamp);
    unsafe {
        if !SIM_READY {
            store::push(true, number, &stamp[..sl], true, store::SmsStatus::Draft, text);
            store::save_all();
            SAW_SENDING = false;
            return store::SmsStatus::Draft;
        }
        if !store::push(true, number, &stamp[..sl], true, store::SmsStatus::Sending, text) {
            return store::SmsStatus::Failed;
        }
        store::save_all();
        let mut cmd = [0u8; 168];
        cmd[..5].copy_from_slice(b"SEND:");
        let nl = number.len().min(20);
        cmd[5..5 + nl].copy_from_slice(&number[..nl]);
        cmd[5 + nl] = 0x1F;
        let tl = text.len().min(120);
        cmd[6 + nl..6 + nl + tl].copy_from_slice(&text[..tl]);
        cmd[6 + nl + tl] = b'\n';
        write_cmd(&cmd[..7 + nl + tl]);
        SAW_SENDING = false;
        store::SmsStatus::Sending
    }
}

/// 轮询 rild 状态：缓存 SIM、检测收件变化并导入、推进发送状态。
/// 返回是否有 UI 可见变化（通知 UI 重绘）。
pub fn poll() -> bool {
    unsafe {
        let mut buf = [0u8; 512];
        let n = read_state(&mut buf);
        let mut changed = false;
        let mut start = 0usize;
        while start < n {
            let end = match buf[start..n].iter().position(|&b| b == b'\n') {
                Some(x) => start + x,
                None => n,
            };
            let line = &buf[start..end];
            if line.starts_with(b"SIM:") {
                SIM_READY = &line[4..] == b"READY";
            } else if line.starts_with(b"INBOX:") {
                if let Some(v) = parse_u32(&line[6..]) {
                    if v != LAST_INBOX {
                        LAST_INBOX = v;
                        if store::import_inbox() > 0 {
                            changed = true;
                        }
                    }
                }
            } else if line.starts_with(b"SMS:") {
                let v = &line[4..];
                let vl = v.len().min(SMS_STATE.len());
                let same = SMS_STATE_LEN == vl && SMS_STATE[..vl] == v[..vl];
                if !same {
                    SMS_STATE_LEN = vl;
                    SMS_STATE[..vl].copy_from_slice(&v[..vl]);
                    if v == b"SENDING" {
                        SAW_SENDING = true;
                    } else if v == b"SENT" && (SAW_SENDING || store::has_sending()) {
                        store::apply_send_result(true);
                        SAW_SENDING = false;
                        changed = true;
                    } else if v == b"FAILED" && (SAW_SENDING || store::has_sending()) {
                        store::apply_send_result(false);
                        SAW_SENDING = false;
                        changed = true;
                    } else if v == b"IDLE" && SAW_SENDING {
                        SAW_SENDING = false;
                    }
                }
            }
            start = end + 1;
        }
        changed
    }
}

/// 初始化 modem 接口缓存（首次轮询）。
pub fn init() {
    unsafe {
        SIM_READY = false;
        LAST_INBOX = 0;
        SMS_STATE_LEN = 0;
        SAW_SENDING = false;
    }
    poll();
}