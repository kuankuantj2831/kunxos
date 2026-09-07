//! L2CAP 层：基本头（Basic Header）解析/构造与 LE Credit Based（面向连接）信令。
//!
//! 基本帧：`[len(2 LE), cid(2 LE), payload...]`。
//! SIG（CID 0x0005）通道内信令载荷：`[code(1), id(1), len(2 LE), data...]`。
//! 所有函数均为纯函数，越界返回 `None`，绝不 panic。

/// 固定通道（LE）。
pub const CID_NULL: u16 = 0x0000;
pub const CID_ATT: u16 = 0x0004;
pub const CID_SMP: u16 = 0x0006;
pub const CID_SIG: u16 = 0x0005;

/// 解析 L2CAP 基本头：返回 `(长度, CID, payload)`。
///
/// 帧长字段与实际可用字节数不符（截断）或不足 4 字节返回 `None`。
pub fn parse_basic(frame: &[u8]) -> Option<(u16, u16, &[u8])> {
    if frame.len() < 4 {
        return None;
    }
    let len = u16::from_le_bytes([frame[0], frame[1]]);
    let cid = u16::from_le_bytes([frame[2], frame[3]]);
    let plen = len as usize;
    if 4 + plen > frame.len() {
        return None;
    }
    Some((len, cid, &frame[4..4 + plen]))
}

/// 构造 L2CAP 基本帧（含 4 字节头）写入 out，返回长度。
///
/// `payload` 超过 65535 字节或 `out` 空间不足返回 `None`。
pub fn build_basic(cid: u16, payload: &[u8], out: &mut [u8]) -> Option<usize> {
    if payload.len() > u16::MAX as usize {
        return None;
    }
    let n = 4 + payload.len();
    if out.len() < n {
        return None;
    }
    out[0..2].copy_from_slice(&(payload.len() as u16).to_le_bytes());
    out[2..4].copy_from_slice(&cid.to_le_bytes());
    out[4..n].copy_from_slice(payload);
    Some(n)
}

/// LE Credit Based 信令命令码（SIG 通道内）。
pub const CMD_LE_CB_REQ: u8 = 0x14;
pub const CMD_LE_CB_RSP: u8 = 0x15;

/// 标准 PSM：ATT。
pub const PSM_ATT: u16 = 0x001F;

/// 解析 LE Credit Based Connection Request（SIG 通道内载荷）：
/// 返回 `(psm, scid, mtu)`。
///
/// 载荷布局：`[code(1)=0x14, id(1), len(2 LE), data: psm(2), scid(2),
/// credits(2), mtu(2)]`。
pub fn parse_le_cb_req(payload: &[u8]) -> Option<(u16, u16, u16)> {
    if payload.len() < 6 || payload[0] != CMD_LE_CB_REQ {
        return None;
    }
    let sig_len = u16::from_le_bytes([payload[2], payload[3]]) as usize;
    if payload.len() < 4 + sig_len {
        return None;
    }
    let data = &payload[4..4 + sig_len];
    if data.len() < 8 {
        return None;
    }
    Some((
        u16::from_le_bytes([data[0], data[1]]), // psm
        u16::from_le_bytes([data[2], data[3]]), // scid
        u16::from_le_bytes([data[6], data[7]]), // mtu
    ))
}

/// 构造 LE Credit Based Connection Response（SIG 通道内载荷）：
/// `[code(1)=0x15, id(1), len(2 LE), data: dcid(2), mtu(2), credits(2),
/// result(2)]`，共 12 字节。
///
/// 注意：本 API 不接收请求 id，固定回显 id=0x01；需要回显真实 id 的调用方可自行改写
/// 输出 `out[1]`。
pub fn build_le_cb_rsp(dcid: u16, mtu: u16, credits: u16, result: u16, out: &mut [u8]) -> Option<usize> {
    if out.len() < 12 {
        return None;
    }
    out[0] = CMD_LE_CB_RSP;
    out[1] = 0x01; // id（固定）
    out[2] = 8; // len (LE)
    out[3] = 0;
    out[4..6].copy_from_slice(&dcid.to_le_bytes());
    out[6..8].copy_from_slice(&mtu.to_le_bytes());
    out[8..10].copy_from_slice(&credits.to_le_bytes());
    out[10..12].copy_from_slice(&result.to_le_bytes());
    Some(12)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_frame_roundtrip() {
        let payload = [0x0A, 0x0B, 0x0C];
        let mut out = [0u8; 32];
        let n = build_basic(CID_ATT, &payload, &mut out).unwrap();
        assert_eq!(n, 7);
        assert_eq!(&out[..4], &[3, 0, 0x04, 0x00]);
        let (len, cid, p) = parse_basic(&out[..n]).unwrap();
        assert_eq!(len, 3);
        assert_eq!(cid, CID_ATT);
        assert_eq!(p, &payload);
        // 空载荷
        let n = build_basic(CID_SMP, &[], &mut out).unwrap();
        assert_eq!(n, 4);
        let (len, cid, p) = parse_basic(&out[..n]).unwrap();
        assert_eq!((len, cid, p.len()), (0, CID_SMP, 0));
        // 截断 / 头不足
        assert!(parse_basic(&out[..n - 1]).is_none());
        assert!(parse_basic(&[0u8; 3]).is_none());
        // 帧长字段大于实际 → None
        let mut bad = [0u8; 6];
        bad[0] = 10;
        assert!(parse_basic(&bad).is_none());
    }

    #[test]
    fn credit_based_signaling() {
        // 构造 SIG 载荷：code=0x14, id=0x07, len=8, psm=0x001F, scid=0x0040,
        // credits=5, mtu=0x0100
        let mut sig = [0u8; 16];
        sig[0] = CMD_LE_CB_REQ;
        sig[1] = 0x07;
        sig[2] = 8;
        sig[3] = 0;
        sig[4..6].copy_from_slice(&PSM_ATT.to_le_bytes());
        sig[6..8].copy_from_slice(&0x0040u16.to_le_bytes());
        sig[8..10].copy_from_slice(&0x0005u16.to_le_bytes());
        sig[10..12].copy_from_slice(&0x0100u16.to_le_bytes());
        let (psm, scid, mtu) = parse_le_cb_req(&sig[..12]).unwrap();
        assert_eq!((psm, scid, mtu), (PSM_ATT, 0x0040, 0x0100));
        // 错误 code
        sig[0] = CMD_LE_CB_RSP;
        assert!(parse_le_cb_req(&sig[..12]).is_none());
        sig[0] = CMD_LE_CB_REQ;
        // 截断（缺 data）
        assert!(parse_le_cb_req(&sig[..6]).is_none());
        // 构造 Response 并逐字段校验
        let mut out = [0u8; 16];
        let n = build_le_cb_rsp(0x0040, 0x0100, 0x0005, 0x0000, &mut out).unwrap();
        assert_eq!(n, 12);
        assert_eq!(out[0], CMD_LE_CB_RSP);
        assert_eq!(u16::from_le_bytes([out[2], out[3]]), 8);
        assert_eq!(u16::from_le_bytes([out[4], out[5]]), 0x0040);
        assert_eq!(u16::from_le_bytes([out[6], out[7]]), 0x0100);
        assert_eq!(u16::from_le_bytes([out[8], out[9]]), 5);
        assert_eq!(u16::from_le_bytes([out[10], out[11]]), 0);
        // 结果字段（拒绝：PSM 不支持 0x0002）
        let n = build_le_cb_rsp(0x0000, 0x0017, 0, 0x0002, &mut out).unwrap();
        assert_eq!(u16::from_le_bytes([out[10], out[11]]), 0x0002);
        assert_eq!(n, 12);
    }
}
