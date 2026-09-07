//! HFP（Hands-Free Profile）：AT 命令子集 + eSCO 音频参数，运行于 RFCOMM。
//!
//! 覆盖：
//! - HF/AG 两侧的核心 AT 命令：AT+BRSF、AT+CIND、AT+CMER、AT+CHLD、ATD、
//!   ATA、AT+CHUP、AT+VGS/VGM、AT+CLCC、AT+VTS、AT+BVRA、AT+NREC、AT+BCC/BCS
//! - AG 响应解析（+BRSF:、+CIND:、+CIEV、OK/ERROR、RING）
//! - 指示器（SERVICE/CALL/CALLSETUP/CALLHELD/SIGNAL/ROAM/BATTCHG）与 CIEV 事件
//! - eSCO（增强同步连接）参数：S3（CVSD）与 HCI Enhanced Setup Sync Connection，
//!   配合音频延迟优化（`estimated_latency_ms`）
//!
//! AT 行以 CRLF 分隔。纯函数、无堆、越界返回 `None`、绝不 panic。

// ==================== 特征位（AT+BRSF） ====================

pub const BRSF_EC_NR: u32 = 1 << 0;
pub const BRSF_CALL_WAITING_3WAY: u32 = 1 << 1;
pub const BRSF_CLI_PRESENTATION: u32 = 1 << 2;
pub const BRSF_VOICE_RECOGNITION: u32 = 1 << 3;
pub const BRSF_REMOTE_VOLUME_CTRL: u32 = 1 << 4;
pub const BRSF_ENHANCED_CALL_STATUS: u32 = 1 << 5;
pub const BRSF_ENHANCED_CALL_CONTROL: u32 = 1 << 6;
pub const BRSF_CODEC_NEGOTIATION: u32 = 1 << 7;
pub const BRSF_HF_INDICATORS: u32 = 1 << 8;
pub const BRSF_ESCO_S4: u32 = 1 << 9;

// ==================== 指示器 ====================

pub const IND_SERVICE: u8 = 1;
pub const IND_CALL: u8 = 2;
pub const IND_CALLSETUP: u8 = 3;
pub const IND_CALLHELD: u8 = 4;
pub const IND_SIGNAL: u8 = 5;
pub const IND_ROAM: u8 = 6;
pub const IND_BATTCHG: u8 = 7;

/// 当前指示器集合。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Indicators {
    pub service: u8,
    pub call: u8,
    pub callsetup: u8,
    pub callheld: u8,
    pub signal: u8,
    pub roam: u8,
    pub battchg: u8,
}

impl Indicators {
    pub fn new() -> Self {
        Indicators {
            service: 0,
            call: 0,
            callsetup: 0,
            callheld: 0,
            signal: 3,
            roam: 0,
            battchg: 5,
        }
    }

    /// 把单个指示器变化格式化为 `+CIEV: <ind>,<val>\r\n`。
    pub fn to_ciev(&self, ind: u8, value: u8, out: &mut [u8]) -> Option<usize> {
        let mut buf = [0u8; 24];
        let head = match ind {
            IND_SERVICE => b"+CIEV: 1,".as_slice(),
            IND_CALL => b"+CIEV: 2,".as_slice(),
            IND_CALLSETUP => b"+CIEV: 3,".as_slice(),
            IND_CALLHELD => b"+CIEV: 4,".as_slice(),
            IND_SIGNAL => b"+CIEV: 5,".as_slice(),
            IND_ROAM => b"+CIEV: 6,".as_slice(),
            _ => b"+CIEV: 7,".as_slice(),
        };
        buf[..head.len()].copy_from_slice(head);
        let mut n = head.len();
        let v = (value % 100) as u64;
        let (digits, start) = dec_digits(v);
        let dl = 20 - start;
        buf[n..n + dl].copy_from_slice(&digits[start..20]);
        n += dl;
        buf[n] = b'\r';
        buf[n + 1] = b'\n';
        n += 2;
        if out.len() < n {
            return None;
        }
        out[..n].copy_from_slice(&buf[..n]);
        Some(n)
    }
}

// ==================== 通用辅助 ====================

/// 十进制数字（右对齐到 20 字节数组），返回 `(arr, start)`。
pub fn dec_digits(mut v: u64) -> ([u8; 20], usize) {
    let mut b = [0u8; 20];
    let mut i = 20usize;
    if v == 0 {
        b[19] = b'0';
        return (b, 19);
    }
    while v > 0 {
        i -= 1;
        b[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    (b, i)
}

/// 把后缀 CRLF 追加到 AT/BRSF 命令行。
fn append_crlf(at: &[u8], out: &mut [u8]) -> Option<usize> {
    let n = at.len();
    if out.len() < n + 2 {
        return None;
    }
    out[..n].copy_from_slice(at);
    out[n] = b'\r';
    out[n + 1] = b'\n';
    Some(n + 2)
}

// ==================== AT 命令构造（HF → AG） ====================

pub fn at_supported_features(features: u32) -> [u8; 32] {
    let mut b = [0u8; 32];
    let head = b"AT+BRSF=";
    b[..head.len()].copy_from_slice(head);
    let mut n = head.len();
    let (digits, start) = dec_digits(features as u64);
    let dl = 20 - start;
    b[n..n + dl].copy_from_slice(&digits[start..20]);
    n += dl;
    b[n] = b'\r';
    b[n + 1] = b'\n';
    b
}

pub fn at_indicator_list() -> [u8; 24] {
    let mut b = [0u8; 24];
    let s = b"AT+CIND?";
    b[..s.len()].copy_from_slice(s);
    b[s.len()] = b'\r';
    b[s.len() + 1] = b'\n';
    b
}

pub fn at_enable_indicator_reporting() -> [u8; 32] {
    let mut b = [0u8; 32];
    let s = b"AT+CMER=3,0,0,1\r\n";
    b[..s.len()].copy_from_slice(s);
    b
}

pub fn at_dial(number: &[u8]) -> [u8; 40] {
    let mut b = [0u8; 40];
    let head = b"ATD";
    b[..head.len()].copy_from_slice(head);
    let mut n = head.len();
    let dl = number.len().min(34);
    b[n..n + dl].copy_from_slice(&number[..dl]);
    n += dl;
    b[n] = b'\r';
    b[n + 1] = b'\n';
    b
}

pub fn at_answer() -> [u8; 8] {
    let mut b = [0u8; 8];
    let s = b"ATA";
    b[..3].copy_from_slice(s);
    b[3] = b'\r';
    b[4] = b'\n';
    b
}

pub fn at_hangup() -> [u8; 12] {
    let mut b = [0u8; 12];
    let s = b"AT+CHUP";
    b[..7].copy_from_slice(s);
    b[7] = b'\r';
    b[8] = b'\n';
    b
}

pub fn at_volume_gain(level: u8) -> [u8; 16] {
    let mut b = [0u8; 16];
    let head = b"AT+VGS=";
    b[..7].copy_from_slice(head);
    let (digits, start) = dec_digits((level % 15) as u64);
    let dl = 20 - start;
    b[7..7 + dl].copy_from_slice(&digits[start..20]);
    b[7 + dl] = b'\r';
    b[7 + dl + 1] = b'\n';
    b
}

/// 通用命令行构造到 `out`，返回长度（含 CRLF）。
pub fn build_at_command(command: &[u8], out: &mut [u8]) -> Option<usize> {
    append_crlf(command, out)
}

// ==================== AG 响应解析 ====================

pub const RSP_OK: u8 = 0;
pub const RSP_ERROR: u8 = 1;
pub const RSP_BRSF: u8 = 2;
pub const RSP_CIND: u8 = 3;
pub const RSP_CIEV: u8 = 4;
pub const RSP_RING: u8 = 5;
pub const RSP_UNKNOWN: u8 = 6;

pub struct AtResponse {
    pub kind: u8,
    pub val: u32,
    pub data: [u8; 128],
    pub data_len: usize,
}

pub fn parse_response(line: &[u8]) -> AtResponse {
    let mut r = AtResponse {
        kind: RSP_UNKNOWN,
        val: 0,
        data: [0u8; 128],
        data_len: 0,
    };
    if line.len() < 2 {
        return r;
    }
    if line.starts_with(b"OK") {
        r.kind = RSP_OK;
    } else if line.starts_with(b"ERROR") {
        r.kind = RSP_ERROR;
    } else if line.starts_with(b"RING") {
        r.kind = RSP_RING;
    } else if line.starts_with(b"+BRSF:") {
        r.kind = RSP_BRSF;
        r.val = parse_u32_after(&line[6..]);
    } else if line.starts_with(b"+CIND:") {
        r.kind = RSP_CIND;
        r.val = count_commas(&line[6..]) as u32;
    } else if line.starts_with(b"+CIEV:") {
        r.kind = RSP_CIEV;
        r.data_len = line.len().min(128);
        r.data[..r.data_len].copy_from_slice(&line[..r.data_len]);
    } else {
        let n = line.len().min(128);
        r.data[..n].copy_from_slice(&line[..n]);
        r.data_len = n;
    }
    r
}

fn parse_u32_after(s: &[u8]) -> u32 {
    let mut v = 0u32;
    for &c in s.iter().take(10) {
        match c {
            b'0'..=b'9' => v = v.saturating_mul(10).saturating_add((c - b'0') as u32),
            _ => {}
        }
    }
    v
}

fn count_commas(s: &[u8]) -> usize {
    s.iter().filter(|&&c| c == b',').count()
}

// ==================== eSCO / HCI 同步连接参数 ====================

pub const ESCO_CVSD: u8 = 0x00;
pub const ESCO_TRANSPARENT: u8 = 0x01;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EscoParams {
    pub transmit_bandwidth: u32,
    pub receive_bandwidth: u32,
    pub max_latency: u16,
    pub voice_setting: u16, // 0x0060 = CVSD
    pub retransmission_effort: u8,
    pub packet_type: u16,
}

/// S3 参数（CVSD，低延迟）。
pub fn esco_s3() -> EscoParams {
    EscoParams {
        transmit_bandwidth: 8000,
        receive_bandwidth: 8000,
        max_latency: 13,
        voice_setting: 0x0060,
        retransmission_effort: 1,
        packet_type: 0x0038, // EV3 | EV4 | EV5
    }
}

/// S1 参数（CVSD，标准）。
pub fn esco_s1() -> EscoParams {
    EscoParams {
        transmit_bandwidth: 8000,
        receive_bandwidth: 8000,
        max_latency: 8,
        voice_setting: 0x0060,
        retransmission_effort: 2,
        packet_type: 0x0008, // EV3
    }
}

/// 构造 HCI Enhanced Setup Synchronous Connection 命令（OGF 0x04 / OCF 0x1D）。
pub fn build_esco_setup(conn_handle: u16, p: &EscoParams, out: &mut [u8]) -> Option<usize> {
    let mut params = [0u8; 21];
    params[0..2].copy_from_slice(&conn_handle.to_le_bytes());
    params[2..6].copy_from_slice(&p.transmit_bandwidth.to_le_bytes());
    params[6..10].copy_from_slice(&p.receive_bandwidth.to_le_bytes());
    params[10..12].copy_from_slice(&p.max_latency.to_le_bytes());
    params[12] = p.voice_setting as u8;
    params[13] = (p.voice_setting >> 8) as u8;
    // 14..18：input/output coding、data format、air 模式全部按 CVSD 默认 0
    params[18] = p.retransmission_effort;
    params[19..21].copy_from_slice(&p.packet_type.to_le_bytes());
    crate::hci::build_command(crate::hci::opcode(0x04, 0x1D), &params, out)
}

/// 延迟优化：由 max_latency 与重传努力估算（毫秒）。
pub fn estimated_latency_ms(p: &EscoParams) -> u32 {
    p.max_latency as u32 + (p.retransmission_effort as u32) * 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_command_builders() {
        let b = at_supported_features(BRSF_ENHANCED_CALL_STATUS | BRSF_ENHANCED_CALL_CONTROL);
        assert_eq!(&b[..10], b"AT+BRSF=96");
        assert_eq!(b[10], b'\r');
        assert_eq!(b[11], b'\n');
        let b = at_indicator_list();
        assert!(b.starts_with(b"AT+CIND?"));
        let b = at_enable_indicator_reporting();
        assert!(b.starts_with(b"AT+CMER=3,0,0,1"));
        let b = at_dial(b"1234567890");
        assert!(b.starts_with(b"ATD1234567890"));
        assert_eq!(b[13], b'\r');
        let b = at_answer();
        assert!(b.starts_with(b"ATA"));
        let b = at_hangup();
        assert!(b.starts_with(b"AT+CHUP"));
        let b = at_volume_gain(7);
        assert!(b.starts_with(b"AT+VGS=7"));
    }

    #[test]
    fn ag_response_parse() {
        let r = parse_response(b"+BRSF: 96");
        assert_eq!(r.kind, RSP_BRSF);
        assert_eq!(r.val, 96);
        let r = parse_response(b"+CIND: 1,0,0,0,3,0,5");
        assert_eq!(r.kind, RSP_CIND);
        assert_eq!(r.val, 6);
        let r = parse_response(b"+CIEV: 3,2");
        assert_eq!(r.kind, RSP_CIEV);
        let r = parse_response(b"OK");
        assert_eq!(r.kind, RSP_OK);
        let r = parse_response(b"RING");
        assert_eq!(r.kind, RSP_RING);
        let r = parse_response(b"+CME ERROR: 4");
        assert_eq!(r.kind, RSP_UNKNOWN);
    }

    #[test]
    fn esco_params_and_setup() {
        let s3 = esco_s3();
        assert_eq!(s3.max_latency, 13);
        assert_eq!(s3.voice_setting, 0x0060);
        let mut out = [0u8; 32];
        let n = build_esco_setup(0x0042, &s3, &mut out).unwrap();
        assert_eq!(&out[..4], &[0x01, 0x1D, 0x10, 21]);
        assert_eq!(u16::from_le_bytes([out[4], out[5]]), 0x0042);
        assert!(estimated_latency_ms(&s3) >= 13);
    }

    #[test]
    fn ciev_indicator_format() {
        let ind = Indicators::new();
        let mut out = [0u8; 32];
        let n = ind.to_ciev(IND_CALLSETUP, 2, &mut out).unwrap();
        assert_eq!(&out[..10], b"+CIEV: 3,2");
        assert_eq!(out[10], b'\r');
        assert_eq!(out[11], b'\n');
        let _ = n;
    }
}