//! AT 命令通道：/dev/modem 的读写 + 行缓冲 + 响应/URC 解析。
//!
//! rild 单线程事件循环：主循环周期调用 [`At::poll_urc`] 读取调制解调器字节
//! 并派发 URC；需要命令-响应交互时调用 [`At::command`]（内部轮询直至最终
//! 结果或超时）。AT+CMGS 需两阶段（先发命令等 "> " 提示符，再发 PDU+0x1A），
//! 用 [`At::command_with_data`] 处理。

use crate::sys;
use crate::sys::SYS_READ;
use crate::sys::SYS_WRITE;

/// 命令最终结果。
#[derive(Clone, Copy, PartialEq)]
pub enum Final {
    Ok,
    Error,
    Cme(u32),
    Cms(u32),
    Timeout,
    NoModem,
}

/// 单行缓冲上限（AT 命令 / URC 行）。
const LINE_CAP: usize = 160;
/// 命令响应缓冲上限（+CMGR 的 PDU 行等）。
pub const RESP_CAP: usize = 512;
/// 命令超时（毫秒）。
const CMD_TIMEOUT_MS: u64 = 3000;

pub struct At {
    pub fd: isize,
    /// 行累积缓冲。
    line: [u8; LINE_CAP],
    line_len: usize,
    /// 是否有挂起命令。
    pending: bool,
    /// 是否等待 "> " 提示符（CMGS）。
    prompt: bool,
    /// 是否已出现 ">"（等待空格确认提示符）。
    prompt_gt: bool,
    /// 提示符已收到、数据已发送，等待最终结果。
    data_sent: bool,
    /// 最终结果。
    final_result: Final,
    /// 响应缓冲（command 调用方提供）。
    resp: *mut u8,
    resp_cap: usize,
    resp_len: usize,
    /// 待发送的 CMGS 数据（command_with_data 提供）。
    data: *const u8,
    data_len: usize,
}

impl At {
    pub const fn new() -> At {
        At {
            fd: -1,
            line: [0; LINE_CAP],
            line_len: 0,
            pending: false,
            prompt: false,
            prompt_gt: false,
            data_sent: false,
            final_result: Final::NoModem,
            resp: core::ptr::null_mut(),
            resp_cap: 0,
            resp_len: 0,
            data: core::ptr::null(),
            data_len: 0,
        }
    }

    /// 打开 /dev/modem（O_RDWR）。
    pub fn open_modem(&mut self) {
        self.fd = unsafe { sys::open("/dev/modem", sys::O_RDWR) };
    }

    fn write_bytes(&self, data: &[u8]) {
        if self.fd >= 0 && !data.is_empty() {
            unsafe {
                sys::syscall3(SYS_WRITE, self.fd as usize, data.as_ptr() as usize, data.len());
            }
        }
    }

    /// 发送 AT 命令并等待最终结果，`resp` 接收中间响应行（不含 URC 与最终行）。
    pub fn command(&mut self, cmd: &[u8], resp: &mut [u8]) -> Final {
        if self.fd < 0 {
            return Final::NoModem;
        }
        let mut out = [0u8; LINE_CAP];
        let n = cmd.len().min(LINE_CAP - 2);
        out[..n].copy_from_slice(&cmd[..n]);
        out[n] = b'\r';
        out[n + 1] = b'\n';
        self.write_bytes(&out[..n + 2]);
        self.begin_wait(resp, false, &[])
    }

    /// 发送 AT+CMGS 类两阶段命令：先发命令，收到 "> " 提示符后发 `data` + 0x1A。
    pub fn command_with_data(&mut self, cmd: &[u8], data: &[u8], resp: &mut [u8]) -> Final {
        if self.fd < 0 {
            return Final::NoModem;
        }
        let mut out = [0u8; LINE_CAP];
        let n = cmd.len().min(LINE_CAP - 2);
        out[..n].copy_from_slice(&cmd[..n]);
        out[n] = b'\r';
        out[n + 1] = b'\n';
        self.write_bytes(&out[..n + 2]);
        self.begin_wait(resp, true, data)
    }

    fn begin_wait(&mut self, resp: &mut [u8], prompt: bool, data: &[u8]) -> Final {
        self.pending = true;
        self.prompt = prompt;
        self.prompt_gt = false;
        self.data_sent = false;
        self.line_len = 0;
        self.final_result = Final::NoModem;
        self.resp = resp.as_mut_ptr();
        self.resp_cap = resp.len();
        self.resp_len = 0;
        self.data = data.as_ptr();
        self.data_len = data.len();
        self.wait_final()
    }

    fn wait_final(&mut self) -> Final {
        let deadline = sys::now_us() + CMD_TIMEOUT_MS * 1000;
        loop {
            self.poll_read();
            if !self.pending {
                return self.final_result;
            }
            if sys::now_us() >= deadline {
                self.pending = false;
                crate::log("[rild] AT command timeout\n");
                return Final::Timeout;
            }
            sys::sleep_ms(10);
        }
    }

    /// 主循环空闲期轮询：读取字节并派发 URC（无挂起命令时）。
    pub fn poll_urc(&mut self) {
        self.poll_read();
    }

    /// 从 /dev/modem 读取全部可用字节并喂给行解析器。
    fn poll_read(&mut self) {
        if self.fd < 0 {
            return;
        }
        loop {
            let mut buf = [0u8; 128];
            let n = unsafe {
                sys::syscall3(SYS_READ, self.fd as usize, buf.as_mut_ptr() as usize, buf.len())
            };
            // syscall 返回原始寄存器值：读失败 -1 在 usize 下是天文数字，
            // 必须先按 isize 判负，否则 &buf[..n] 越界 panic
            let n = n as isize;
            if n <= 0 {
                break;
            }
            let n = n as usize;
            for &b in &buf[..n] {
                self.feed_byte(b);
            }
        }
    }

    fn feed_byte(&mut self, b: u8) {
        // CMGS 数据阶段：等 "> " 提示符（无行尾换行）
        if self.pending && self.prompt && !self.data_sent {
            if b == b'>' {
                self.prompt_gt = true;
                return;
            }
            if self.prompt_gt && b == b' ' {
                // 收到提示符：发送 PDU + 0x1A，随后等最终结果行
                self.data_sent = true;
                self.prompt_gt = false;
                self.line_len = 0;
                if self.data_len > 0 {
                    let data = unsafe { core::slice::from_raw_parts(self.data, self.data_len) };
                    self.write_bytes(data);
                }
                self.write_bytes(&[0x1A]);
                return;
            }
            self.prompt_gt = false;
        }
        if b == b'\n' {
            // 行结束（去尾 \r），拷贝到局部缓冲后处理（避免借用冲突）
            let mut end = self.line_len;
            if end > 0 && self.line[end - 1] == b'\r' {
                end -= 1;
            }
            let mut tmp = [0u8; LINE_CAP];
            tmp[..end].copy_from_slice(&self.line[..end]);
            self.line_len = 0;
            self.handle_line(&tmp[..end]);
        } else if b != b'\r' {
            if self.line_len < LINE_CAP {
                self.line[self.line_len] = b;
                self.line_len += 1;
            }
        }
    }

    fn handle_line(&mut self, line: &[u8]) {
        if self.pending {
            if let Some(f) = parse_final_line(line) {
                self.final_result = f;
                self.pending = false;
                return;
            }
            // 普通响应行：拷入响应缓冲
            if !line.is_empty() && self.resp_len + line.len() + 1 <= self.resp_cap {
                unsafe {
                    core::ptr::copy_nonoverlapping(line.as_ptr(), self.resp.add(self.resp_len), line.len());
                    *self.resp.add(self.resp_len + line.len()) = b'\n';
                }
                self.resp_len += line.len() + 1;
            }
            return;
        }
        // 无挂起命令：URC
        if !line.is_empty() {
            crate::ril::on_urc(line);
        }
    }
}

/// 解析最终结果行：OK / ERROR / +CME ERROR: n / +CMS ERROR: n。
pub fn parse_final_line(line: &[u8]) -> Option<Final> {
    if line.eq_ignore_ascii_case(b"OK") {
        return Some(Final::Ok);
    }
    if line.eq_ignore_ascii_case(b"ERROR") {
        return Some(Final::Error);
    }
    if let Some(rest) = strip_prefix_ci(line, b"+CME ERROR:") {
        return Some(Final::Cme(parse_num(rest).unwrap_or(0)));
    }
    if let Some(rest) = strip_prefix_ci(line, b"+CMS ERROR:") {
        return Some(Final::Cms(parse_num(rest).unwrap_or(0)));
    }
    None
}

fn strip_prefix_ci<'a>(s: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    if s.len() >= prefix.len()
        && s[..prefix.len()].iter().zip(prefix).all(|(a, b)| a.eq_ignore_ascii_case(b))
    {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

/// 解析十进制数。
pub fn parse_num(s: &[u8]) -> Option<u32> {
    let s = s.trim_ascii();
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

/// 提取一行中冒号后的内容（如 "+CRING: 138..." → "138..."），去引号去空白。
pub fn after_colon(line: &[u8]) -> &[u8] {
    let idx = line.iter().position(|&b| b == b':').unwrap_or(line.len());
    let rest = &line[idx + 1..];
    let rest = rest.trim_ascii();
    // 单个 '"'（len==1）时 starts_with 与 ends_with 同真，&rest[1..0] 会
    // panic —— 长度 >= 2 才做去引号
    let rest = if rest.len() >= 2 && rest.starts_with(b"\"") && rest.ends_with(b"\"") {
        &rest[1..rest.len() - 1]
    } else {
        rest
    };
    rest
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
