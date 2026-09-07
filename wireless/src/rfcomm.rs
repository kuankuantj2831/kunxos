//! RFCOMM（蓝牙串口仿真，TS 07.10）复用器：HFP 的传输层。
//!
//! 覆盖：
//! - 帧头：地址（EA/C/R/DLCI）、控制（SABM/UA/DM/DISC/UIH）、长度（EA 多字节）
//! - FCS：CRC-8（多项式 0x07，初值 0xFF，TS 07.10 §5.2.2）
//! - 复用器命令（DLCI 0 上的 UIH）：PN（参数协商）、MSC（调制解调器状态）、
//!   FCON/FCOFF、TEST、NSC、CLD
//! - DLCI 分配与信用流控（credit-based flow）
//!
//! L2CAP PSM 0x0003 承载 RFCOMM 帧；本模块只做帧编解码与复用器逻辑，
//! 与 L2CAP 的接缝由上层（btd 守护进程）负责。纯函数、无堆、绝不 panic。

/// RFCOMM over L2CAP 的 PSM。
pub const PSM_RFCOMM: u16 = 0x0003;

/// 复用器控制通道 DLCI。
pub const DLCI_CONTROL: u8 = 0;
/// 数据通道方向位：DCE→DTE。
pub const DIR_DCE_TO_DTE: u8 = 1;
pub const DIR_DTE_TO_DCE: u8 = 0;

/// 根据服务通道号与方向计算 DLCI（`sc` 1..30）。控制通道 DLCI=0。
pub const fn dlci_for(sc: u8, initiator: bool) -> u8 {
    if sc == 0 {
        DLCI_CONTROL
    } else {
        (sc << 1) | if initiator { DIR_DCE_TO_DTE } else { DIR_DTE_TO_DCE }
    }
}

/// 多路复用器命令码（UIH 载荷第一字节）。
pub const CMD_RLS: u8 = 0x14;
pub const CMD_PN: u8 = 0x80;
pub const CMD_CLD: u8 = 0x10;
pub const CMD_TEST: u8 = 0xE1;
pub const CMD_FCON: u8 = 0xA1;
pub const CMD_FCOFF: u8 = 0xA2;
pub const CMD_MSC: u8 = 0xE3;
pub const CMD_NSC: u8 = 0x18;
pub const CMD_RPN: u8 = 0x92;

/// 无确认的 UIH 响应（非命令）标志：命令 C/R=1，响应 C/R=0。
pub const CR_CMD: u8 = 1;
pub const CR_RSP: u8 = 0;

// ==================== 寻址/控制/长度 ====================

/// 地址字节：`[EA(1) | C/R(1) | DLCI(6)]`。
#[inline]
pub fn build_addr(dlci: u8, cr: u8) -> u8 {
    ((dlci & 0x3F) << 2) | ((cr & 1) << 1) | 1 // EA=1
}

/// 解析地址字节，返回 `(dlci, cr, ea)`。
pub fn parse_addr(a: u8) -> (u8, u8, bool) {
    (a >> 2, (a >> 1) & 1, a & 1 != 0)
}

/// 控制字节。
pub const CTL_SABM: u8 = 0x2F;
pub const CTL_UA: u8 = 0x63;
pub const CTL_DM: u8 = 0x0F;
pub const CTL_DISC: u8 = 0x43;
pub const CTL_UIH: u8 = 0xEF;
/// P/F 位。
pub const P_F: u8 = 0x10;

pub fn is_sabm(c: u8) -> bool {
    (c & 0xEF) == CTL_SABM
}
pub fn is_ua(c: u8) -> bool {
    (c & 0xEF) == CTL_UA
}
pub fn is_dm(c: u8) -> bool {
    (c & 0xEF) == CTL_DM
}
pub fn is_disc(c: u8) -> bool {
    (c & 0xEF) == CTL_DISC
}
pub fn is_uih(c: u8) -> bool {
    (c & 0xEF) == CTL_UIH
}

/// 长度编码（TS 07.10）：单字节 `len <= 127`：`byte = (len<<1) | EA`；
/// 双字节 `len <= 16383`：`byte0 = (低7位<<1)`，`byte1 = (高7位<<1) | 1`。
pub fn encode_len(len: usize) -> ([u8; 2], usize) {
    if len <= 127 {
        ([(len as u8) << 1 | 1, 0], 1)
    } else if len <= 0x3FFF {
        ([
            (((len & 0x7F) as u8) << 1),
            (((len >> 7) as u8) << 1) | 1,
        ], 2)
    } else {
        ([0x01, 0], 1) // 不支持 >=16384，返回占位空长度
    }
}

pub fn decode_len(b: &[u8]) -> Option<(usize, usize)> {
    if b.is_empty() {
        return None;
    }
    let low = (b[0] >> 1) as usize;
    if b[0] & 1 != 0 {
        return Some((low, 1));
    }
    if b.len() < 2 {
        return None;
    }
    let high = (b[1] >> 1) as usize;
    Some((low | (high << 7), 2))
}

// ==================== FCS（TS 07.10 §5.2.2） ====================

/// 计算 8 位 FCS：CRC-8，生成多项式 0x07（x^8+x^2+x+1），初值 0xFF。
pub fn fcs(data: &[u8]) -> u8 {
    let mut crc = 0xFFu8;
    for &b in data {
        crc ^= b;
        for _ in 0..8 {
            if crc & 0x80 != 0 {
                crc = (crc << 1) ^ 0x07;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

pub fn fcs_ok(frame: &[u8], fcs_byte: u8) -> bool {
    // 帧内容（地址、控制、长度、信息）后校验 FCS。
    fcs(frame) == fcs_byte
}

// ==================== 帧构造/解析 ====================

/// 构造普通帧（SABM/UA/DM/DISC）：`addr, ctl, [len], fcs`。
pub fn build_frame(dlci: u8, cr: u8, ctl: u8, pf: bool, out: &mut [u8]) -> Option<usize> {
    let addr = build_addr(dlci, cr);
    let c = ctl | (if pf { P_F } else { 0 });
    let (len, nlen) = encode_len(0);
    let need = 2 + nlen + 1;
    if out.len() < need {
        return None;
    }
    out[0] = addr;
    out[1] = c;
    out[2..2 + nlen].copy_from_slice(&len[..nlen]);
    let body = &out[..2 + nlen];
    let f = fcs(body);
    out[2 + nlen] = f;
    Some(need)
}

/// 构造 UIH 数据帧（或复用器命令，DLCI=0）：`addr, ctl(UIH), len, info, fcs`。
pub fn build_uih(dlci: u8, cr: u8, info: &[u8], out: &mut [u8]) -> Option<usize> {
    let addr = build_addr(dlci, cr);
    let (len, nlen) = encode_len(info.len());
    let need = 2 + nlen + info.len() + 1;
    if out.len() < need {
        return None;
    }
    out[0] = addr;
    out[1] = CTL_UIH;
    out[2..2 + nlen].copy_from_slice(&len[..nlen]);
    let start = 2 + nlen;
    out[start..start + info.len()].copy_from_slice(info);
    let body = &out[..start + info.len()];
    out[start + info.len()] = fcs(body);
    Some(need)
}

/// 解析一帧：返回 `(dlci, cr, ctl, info, fcs_ok)`。
/// 只支持单/双字节长度；截断或不合法返回 `None`。
pub fn parse_frame(frame: &[u8]) -> Option<(u8, u8, u8, &[u8], bool)> {
    if frame.len() < 2 {
        return None;
    }
    let (dlci, cr, _ea) = parse_addr(frame[0]);
    let ctl = frame[1];
    let (flen, used) = decode_len(&frame[2..])?;
    if frame.len() < 2 + used + flen + 1 {
        return None;
    }
    let info_start = 2 + used;
    let info = &frame[info_start..info_start + flen];
    let fcs_byte = frame[info_start + flen];
    let ok = fcs_ok(&frame[..info_start + flen], fcs_byte);
    Some((dlci, cr, ctl, info, ok))
}

// ==================== 复用器命令（DLCI 0） ====================

/// PN（参数协商）载荷构造：`[0x80, 8, dlci, frame_type, credit|priority,
/// max_frame_size(2 BE), initial_credits, 0xFF, 0xFF]`。
pub fn build_pn(dlci: u8, max_frame: u16, credits: u8, priority: u8, out: &mut [u8]) -> Option<usize> {
    if out.len() < 11 {
        return None;
    }
    out[0] = CMD_PN;
    out[1] = 8;
    out[2] = dlci;
    out[3] = 0xE0; // frame type：支持 UIH/UI 等（默认）
    out[4] = (0x80) | (priority & 0x3F); // credit-based 使能 + 优先级
    out[5..7].copy_from_slice(&max_frame.to_be_bytes());
    out[7] = credits;
    out[8] = 0xFF;
    out[9] = 0xFF;
    Some(10)
}

/// 解析 PN 载荷（信息部分，含命令字节）。
pub fn parse_pn(info: &[u8]) -> Option<(u8, u16, u8)> {
    if info.len() < 10 || info[0] != CMD_PN || info[1] != 8 {
        return None;
    }
    let dlci = info[2];
    let max_frame = u16::from_be_bytes([info[5], info[6]]);
    let credits = info[7];
    Some((dlci, max_frame, credits))
}

/// MSC（调制解调器状态命令）构造：`[0xE3, 2, dlci, v24_signals]`。
/// V.24 位：EA(0) | FC(1) | RTC(2) | RTR(3) | IC(4) | DV(5) | 保留。
pub fn build_msc(dlci: u8, v24: u8, out: &mut [u8]) -> Option<usize> {
    if out.len() < 6 {
        return None;
    }
    out[0] = CMD_MSC;
    out[1] = 2;
    out[2] = dlci;
    out[3] = (v24 << 2) | 0x01; // EA=1，信号在 bit2..7
    Some(4)
}

/// 解析 MSC，返回 `(dlci, v24_signals)`。
pub fn parse_msc(info: &[u8]) -> Option<(u8, u8)> {
    if info.len() < 4 || info[0] != CMD_MSC || info[1] != 2 {
        return None;
    }
    let v24 = info[3] >> 2;
    Some((info[2], v24))
}

// ==================== 信用流控 ====================

pub const CREDITS_MAX: u16 = 255;

/// 简化信用计数：发送帧时递减，收到 credit 授予时增加。
pub struct CreditQueue {
    pub initial: u16,
    pub available: u16,
}

impl CreditQueue {
    pub fn new(initial: u16) -> Self {
        CreditQueue {
            initial: initial.min(CREDITS_MAX),
            available: initial.min(CREDITS_MAX),
        }
    }

    /// 发送一帧（每帧消耗一个 credit）。没有信用时返回 false（应暂停）。
    pub fn send(&mut self) -> bool {
        if self.available == 0 {
            return false;
        }
        self.available -= 1;
        true
    }

    /// 对端授予 `n` 个信用。
    pub fn grant(&mut self, n: u16) {
        self.available = self.available.saturating_add(n).min(CREDITS_MAX);
    }

    pub fn can_send(&self) -> bool {
        self.available > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addr_and_len_roundtrip() {
        let a = build_addr(dlci_for(2, true), CR_CMD);
        let (dlci, cr, ea) = parse_addr(a);
        assert_eq!(dlci, 5);
        assert_eq!(cr, CR_CMD);
        assert!(ea);
        let (len, used) = encode_len(300);
        assert_eq!(used, 2);
        let (n, u) = decode_len(&len[..used]).unwrap();
        assert_eq!(n, 300);
        assert_eq!(u, 2);
        let (len1, used1) = encode_len(80);
        assert_eq!(used1, 1);
        assert_eq!(decode_len(&len1[..used1]).unwrap().0, 80);
    }

    #[test]
    fn control_frame_roundtrip() {
        let mut b = [0u8; 8];
        let n = build_frame(dlci_for(1, true), CR_CMD, CTL_SABM, true, &mut b).unwrap();
        let (dlci, cr, ctl, info, ok) = parse_frame(&b[..n]).unwrap();
        assert_eq!(dlci, 3);
        assert_eq!(cr, CR_CMD);
        assert!(is_sabm(ctl));
        assert!(info.is_empty());
        assert!(ok);
    }

    #[test]
    fn uih_with_info_roundtrip() {
        let info = [0x80, 8, 3, 0xE0, 0x80, 0x01, 0x00, 0x07, 0xFF, 0xFF];
        let mut b = [0u8; 32];
        let n = build_uih(DLCI_CONTROL, CR_CMD, &info, &mut b).unwrap();
        let (dlci, cr, ctl, info2, ok) = parse_frame(&b[..n]).unwrap();
        assert_eq!(dlci, 0);
        assert!(is_uih(ctl));
        assert_eq!(info2, &info);
        assert!(ok);
        // 篡改 → FCS 失败
        let mut bad = b;
        bad[3] ^= 1;
        let (_, _, _, _, ok2) = parse_frame(&bad[..n]).unwrap();
        assert!(!ok2);
        let _ = cr;
    }

    #[test]
    fn pn_and_msc() {
        let mut b = [0u8; 16];
        let n = build_pn(dlci_for(1, true), 672, 7, 1, &mut b).unwrap();
        let info = &b[..n];
        let (dlci, max_frame, credits) = parse_pn(info).unwrap();
        assert_eq!(dlci, 3);
        assert_eq!(max_frame, 672);
        assert_eq!(credits, 7);
        let n = build_msc(dlci_for(1, true), 0x15, &mut b).unwrap();
        let (d2, v24) = parse_msc(&b[..n]).unwrap();
        assert_eq!(d2, 3);
        assert_eq!(v24, 0x15);
    }

    #[test]
    fn credit_queue() {
        let mut q = CreditQueue::new(4);
        assert!(q.send());
        assert!(q.send());
        assert!(q.send());
        assert!(q.send());
        assert!(!q.can_send());
        q.grant(3);
        assert!(q.can_send());
        assert!(q.send());
    }

    #[test]
    fn fcs_known() {
        // 空内容 FCS 为 0xFF（初值）。
        assert_eq!(fcs(b""), 0xFF);
        assert_eq!(fcs(&[0x00]), 0xF3);
    }
}