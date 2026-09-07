//! 蓝牙 HCI 层：H4 帧编解码、LE 命令构造与 LE Meta 事件解析。
//!
//! 帧格式（多字节字段一律小端）：
//! - 命令：`[0x01, opcode_lo, opcode_hi, param_len, params...]`
//! - 事件：`[0x04, code, len, params...]`
//! - 命令完成事件 (0x0E) 参数：`[num_hci_pkts(1), opcode(2), status(1), ret...]`
//! - 命令状态事件 (0x0F) 参数：`[status(1), num_hci_pkts(1), opcode(2)]`
//!
//! 所有解析/构造均为纯函数，越界返回 `None`，绝不 panic。

/// H4 帧类型。
pub const H4_CMD: u8 = 0x01;
pub const H4_ACL: u8 = 0x02;
pub const H4_SCO: u8 = 0x03;
pub const H4_EVT: u8 = 0x04;

/// 由 OGF/OCF 合成 16 位 opcode：`(ogf << 10) | ocf`。
pub const fn opcode(ogf: u8, ocf: u16) -> u16 {
    ((ogf as u16) << 10) | ocf
}

pub const OGF_LINK_CTRL: u8 = 0x01;
pub const OGF_LINK_POLICY: u8 = 0x02;
pub const OGF_HOST_CTRL: u8 = 0x03;
/// 信息参数（Informational Parameters）OGF：Read BD_ADDR / Read Local Version 属于此类。
pub const OGF_INFORMATIONAL: u8 = 0x04;
pub const OGF_LE: u8 = 0x08;

/// 常用命令 OCF（配合 OGF 使用）。
pub const OCF_RESET: u16 = 0x0003; // 0x0C03
pub const OCF_READ_BD_ADDR: u16 = 0x0009; // 0x1009
pub const OCF_READ_LOCAL_VERSION: u16 = 0x0001; // 0x1001
pub const OCF_LE_SET_SCAN_PARAMS: u16 = 0x000B; // 0x200B
pub const OCF_LE_SET_SCAN_ENABLE: u16 = 0x000C; // 0x200C
pub const OCF_LE_CREATE_CONN: u16 = 0x000D; // 0x200D
pub const OCF_LE_SET_ADV_DATA: u16 = 0x0008; // 0x2008
pub const OCF_LE_SET_ADV_ENABLE: u16 = 0x000A; // 0x200A
pub const OCF_LE_CONN_UPDATE: u16 = 0x0013; // 0x2013

/// 事件码。
pub const EVT_CMD_COMPLETE: u8 = 0x0E;
pub const EVT_CMD_STATUS: u8 = 0x0F;
pub const EVT_LE_META: u8 = 0x3E;

/// LE Meta 子事件。
pub const SUBEVT_CONN_COMPLETE: u8 = 0x01;
pub const SUBEVT_ADV_REPORT: u8 = 0x02;

/// HCI 命令包：把 opcode+参数序列化为 H4 帧（首字节 0x01），写入 out，返回长度。
///
/// 布局：`[0x01, opcode_lo, opcode_hi, param_len, params...]`。
/// `params` 超过 255 字节或 `out` 空间不足时返回 `None`。
pub fn build_command(opcode: u16, params: &[u8], out: &mut [u8]) -> Option<usize> {
    if params.len() > 255 {
        return None; // param_len 是单字节
    }
    let n = 4 + params.len();
    if out.len() < n {
        return None;
    }
    out[0] = H4_CMD;
    out[1] = opcode as u8;
    out[2] = (opcode >> 8) as u8;
    out[3] = params.len() as u8;
    out[4..n].copy_from_slice(params);
    Some(n)
}

/// HCI 事件包解析：返回 `(event_code, params)`。
///
/// 布局：`[0x04, code, len, params...]`；长度不符或非事件帧返回 `None`。
pub fn parse_event(frame: &[u8]) -> Option<(u8, &[u8])> {
    if frame.len() < 3 || frame[0] != H4_EVT {
        return None;
    }
    let len = frame[2] as usize;
    if 3 + len > frame.len() {
        return None;
    }
    Some((frame[1], &frame[3..3 + len]))
}

/// 命令完成事件参数解析：返回 `(opcode, status, return_params)`。
///
/// 参数布局：`[num_hci_pkts(1), opcode(2 LE), status(1), ret...]`。
pub fn parse_cmd_complete(params: &[u8]) -> Option<(u16, u8, &[u8])> {
    if params.len() < 4 {
        return None;
    }
    let op = u16::from_le_bytes([params[1], params[2]]);
    Some((op, params[3], &params[4..]))
}

/// 命令状态事件参数解析：返回 `(status, opcode)`。
///
/// 参数布局：`[status(1), num_hci_pkts(1), opcode(2 LE)]`。
pub fn parse_cmd_status(params: &[u8]) -> Option<(u8, u16)> {
    if params.len() < 4 {
        return None;
    }
    Some((params[0], u16::from_le_bytes([params[2], params[3]])))
}

/// LE 广播报告（一条）。
#[derive(Clone, Copy, Debug)]
pub struct LeAdvReport {
    pub event_type: u8,
    pub addr_type: u8,
    pub addr: [u8; 6],
    pub data: [u8; 31],
    pub data_len: usize,
    pub rssi: i8,
}

/// 解析 LE Advertising Report 事件参数（可含多条），返回条数（最多 4）。
///
/// 参数布局：`[subevent(1), num_reports(1), 每条: event_type(1), addr_type(1),
/// addr(6), data_len(1), data(data_len), rssi(1)]`。数据超 31 字节截断，截断的
/// 报告整体丢弃（停在已解析的完整报告之后）。
pub fn parse_le_adv_reports(params: &[u8], out: &mut [LeAdvReport; 4]) -> usize {
    if params.len() < 2 || params[0] != SUBEVT_ADV_REPORT {
        return 0;
    }
    let num = (params[1] as usize).min(4);
    let mut pos = 2;
    let mut count = 0;
    for _ in 0..num {
        // 最小报告头：event_type(1) + addr_type(1) + addr(6) + data_len(1)
        if pos + 9 > params.len() {
            break;
        }
        let event_type = params[pos];
        let addr_type = params[pos + 1];
        let mut addr = [0u8; 6];
        addr.copy_from_slice(&params[pos + 2..pos + 8]);
        let dlen = params[pos + 8] as usize;
        let report_end = pos + 9 + dlen + 1; // 含 rssi
        if report_end > params.len() {
            break;
        }
        let mut data = [0u8; 31];
        let dl = dlen.min(31);
        data[..dl].copy_from_slice(&params[pos + 9..pos + 9 + dl]);
        let rssi = params[pos + 9 + dlen] as i8;
        out[count] = LeAdvReport {
            event_type,
            addr_type,
            addr,
            data,
            data_len: dl,
            rssi,
        };
        count += 1;
        pos = report_end;
    }
    count
}

/// LE Connection Complete 参数。
#[derive(Clone, Copy, Debug)]
pub struct LeConnComplete {
    pub status: u8,
    pub handle: u16,
    pub role: u8,
    pub peer_addr_type: u8,
    pub peer_addr: [u8; 6],
    pub interval: u16,
}

/// 解析 LE Connection Complete 事件参数。
///
/// 参数布局：`[subevent(1), status(1), handle(2 LE), role(1),
/// peer_addr_type(1), peer_addr(6), interval(2 LE), ...]`。
pub fn parse_le_conn_complete(params: &[u8]) -> Option<LeConnComplete> {
    if params.len() < 14 || params[0] != SUBEVT_CONN_COMPLETE {
        return None;
    }
    let status = params[1];
    let handle = u16::from_le_bytes([params[2], params[3]]);
    let role = params[4];
    let peer_addr_type = params[5];
    let mut peer_addr = [0u8; 6];
    peer_addr.copy_from_slice(&params[6..12]);
    let interval = u16::from_le_bytes([params[12], params[13]]);
    Some(LeConnComplete {
        status,
        handle,
        role,
        peer_addr_type,
        peer_addr,
        interval,
    })
}

/// HCI Reset（无参数）。
pub fn cmd_reset(out: &mut [u8]) -> Option<usize> {
    build_command(opcode(OGF_HOST_CTRL, OCF_RESET), &[], out)
}

/// Read BD_ADDR（无参数，opcode 0x1009）。
pub fn cmd_read_bd_addr(out: &mut [u8]) -> Option<usize> {
    build_command(opcode(OGF_INFORMATIONAL, OCF_READ_BD_ADDR), &[], out)
}

/// Read Local Version Information（无参数，opcode 0x1001）。
pub fn cmd_read_local_version(out: &mut [u8]) -> Option<usize> {
    build_command(opcode(OGF_INFORMATIONAL, OCF_READ_LOCAL_VERSION), &[], out)
}

/// LE Set Scan Parameters：`[scan_type(1), interval(2), window(2),
/// own_addr_type(1)=public, filter_policy(1)=accept all]`。
pub fn cmd_le_set_scan_params(active: bool, interval: u16, window: u16, out: &mut [u8]) -> Option<usize> {
    let mut p = [0u8; 7];
    p[0] = if active { 1 } else { 0 };
    p[1..3].copy_from_slice(&interval.to_le_bytes());
    p[3..5].copy_from_slice(&window.to_le_bytes());
    // p[5] = own_addr_type(public)，p[6] = scanning_filter_policy(accept all) 固定
    build_command(opcode(OGF_LE, OCF_LE_SET_SCAN_PARAMS), &p, out)
}

/// LE Set Scan Enable：`[scan_enable(1), filter_duplicates(1)]`。
pub fn cmd_le_set_scan_enable(enabled: bool, filter_dup: bool, out: &mut [u8]) -> Option<usize> {
    let p = [if enabled { 1 } else { 0 }, if filter_dup { 1 } else { 0 }];
    build_command(opcode(OGF_LE, OCF_LE_SET_SCAN_ENABLE), &p, out)
}

/// LE Create Connection：对端地址可配，其余参数固定
/// （scan_interval=0x0060，scan_window=0x0030，own=public，conn_interval=0x0018，
/// latency=0，supervision_timeout=0x02EE，CE length=0）。
pub fn cmd_le_create_conn(peer_addr_type: u8, peer_addr: &[u8; 6], out: &mut [u8]) -> Option<usize> {
    let mut p = [0u8; 25];
    p[0..2].copy_from_slice(&0x0060u16.to_le_bytes()); // le_scan_interval
    p[2..4].copy_from_slice(&0x0030u16.to_le_bytes()); // le_scan_window
    // p[4] = initiator_filter_policy = 0（由对端地址指定）
    p[5] = peer_addr_type;
    p[6..12].copy_from_slice(peer_addr);
    // p[12] = own_addr_type = 0（public）
    p[13..15].copy_from_slice(&0x0018u16.to_le_bytes()); // conn_interval_min
    p[15..17].copy_from_slice(&0x0018u16.to_le_bytes()); // conn_interval_max
    // p[17..19] = conn_latency = 0
    p[19..21].copy_from_slice(&0x02EEu16.to_le_bytes()); // supervision_timeout
    // p[21..25] = min/max CE length = 0
    build_command(opcode(OGF_LE, OCF_LE_CREATE_CONN), &p, out)
}

/// LE Set Advertising Enable：`[advertising_enable(1)]`。
pub fn cmd_le_set_adv_enable(enabled: bool, out: &mut [u8]) -> Option<usize> {
    let p = [if enabled { 1 } else { 0 }];
    build_command(opcode(OGF_LE, OCF_LE_SET_ADV_ENABLE), &p, out)
}

/// LE Set Advertising Data：`[adv_data_len(1), data(0..=31)]`。
/// `data` 超过 31 字节（LE 广播数据上限）返回 `None`。
pub fn cmd_le_set_adv_data(data: &[u8], out: &mut [u8]) -> Option<usize> {
    if data.len() > 31 {
        return None;
    }
    let mut p = [0u8; 32];
    p[0] = data.len() as u8;
    p[1..1 + data.len()].copy_from_slice(data);
    build_command(opcode(OGF_LE, OCF_LE_SET_ADV_DATA), &p[..1 + data.len()], out)
}

/// LE Connection Update：间隔可配，其余固定（latency=0，
/// supervision_timeout=0x02EE，CE length=0）。
pub fn cmd_le_conn_update(handle: u16, min_interval: u16, max_interval: u16, out: &mut [u8]) -> Option<usize> {
    let mut p = [0u8; 14];
    p[0..2].copy_from_slice(&handle.to_le_bytes());
    p[2..4].copy_from_slice(&min_interval.to_le_bytes());
    p[4..6].copy_from_slice(&max_interval.to_le_bytes());
    // p[6..8] = conn_latency = 0
    p[8..10].copy_from_slice(&0x02EEu16.to_le_bytes()); // supervision_timeout
    // p[10..14] = min/max CE length = 0
    build_command(opcode(OGF_LE, OCF_LE_CONN_UPDATE), &p, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_frame_layout() {
        let mut out = [0u8; 64];
        // Reset → 0x0C03，无参数
        let n = cmd_reset(&mut out).unwrap();
        assert_eq!(n, 4);
        assert_eq!(&out[..4], &[H4_CMD, 0x03, 0x0C, 0x00]);
        // LE Set Scan Params（active）→ 0x200B
        cmd_le_set_scan_params(true, 0x0060, 0x0030, &mut out).unwrap();
        assert_eq!(&out[..4], &[H4_CMD, 0x0B, 0x20, 0x07]);
        assert_eq!(out[4], 1); // active
        assert_eq!(u16::from_le_bytes([out[5], out[6]]), 0x0060);
        assert_eq!(u16::from_le_bytes([out[7], out[8]]), 0x0030);
        assert_eq!(out[9], 0); // own_addr_type public
        assert_eq!(out[10], 0); // filter policy
        // 被动扫描
        let n = cmd_le_set_scan_params(false, 0x0010, 0x0010, &mut out).unwrap();
        assert_eq!(out[4], 0);
        assert_eq!(n, 4 + 7);
        // 参数过长返回 None
        assert!(build_command(0x2008, &[0u8; 256], &mut out).is_none());
    }

    #[test]
    fn event_parsing() {
        // Command Complete：num_pkts=1, opcode=0x200B, status=0, ret=[0x0A, 0x00]
        let mut ev = [0u8; 16];
        ev[0] = H4_EVT;
        ev[1] = EVT_CMD_COMPLETE;
        ev[2] = 6;
        ev[3] = 1;
        ev[4] = 0x0B;
        ev[5] = 0x20;
        ev[6] = 0x00;
        ev[7] = 0x0A;
        ev[8] = 0x00;
        let (code, params) = parse_event(&ev[..9]).unwrap();
        assert_eq!(code, EVT_CMD_COMPLETE);
        let (op, status, rp) = parse_cmd_complete(params).unwrap();
        assert_eq!(op, 0x200B);
        assert_eq!(status, 0);
        assert_eq!(rp, &[0x0A, 0x00]);
        // Command Status：status=0x01, num_pkts=1, opcode=0x0C03
        let mut ev2 = [0u8; 16];
        ev2[0] = H4_EVT;
        ev2[1] = EVT_CMD_STATUS;
        ev2[2] = 4;
        ev2[3] = 0x01;
        ev2[4] = 1;
        ev2[5] = 0x03;
        ev2[6] = 0x0C;
        let (code2, p2) = parse_event(&ev2[..7]).unwrap();
        assert_eq!(code2, EVT_CMD_STATUS);
        let (status2, op2) = parse_cmd_status(p2).unwrap();
        assert_eq!((status2, op2), (0x01, 0x0C03));
        // 截断 / 非事件帧
        assert!(parse_event(&ev[..2]).is_none());
        assert!(parse_event(&[0x01, 0x0E, 0x01, 0x00]).is_none());
        assert!(parse_cmd_complete(&[1, 2, 3]).is_none());
        assert!(parse_cmd_status(&[1, 2, 3]).is_none());
    }

    #[test]
    fn adv_report_parsing() {
        let mut params = [0u8; 64];
        params[0] = SUBEVT_ADV_REPORT;
        params[1] = 2;
        // 报告 1：adv_ind / public / AA..FF / 5 字节数据 / rssi -55
        params[2] = 0x01;
        params[3] = 0x00;
        params[4..10].copy_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
        params[10] = 5;
        params[11..16].copy_from_slice(&[0x02, 0x01, 0x06, 0x03, 0x19]);
        params[16] = 0xC9u8; // -55
        // 报告 2：scan_rsp / random / 01..06 / 2 字节数据 / rssi -49
        params[17] = 0x04;
        params[18] = 0x01;
        params[19..25].copy_from_slice(&[1, 2, 3, 4, 5, 6]);
        params[25] = 2;
        params[26..28].copy_from_slice(&[0x00, 0x00]);
        params[28] = 0xCFu8; // -49
        let mut out = [LeAdvReport {
            event_type: 0,
            addr_type: 0,
            addr: [0u8; 6],
            data: [0u8; 31],
            data_len: 0,
            rssi: 0,
        }; 4];
        let n = parse_le_adv_reports(&params[..29], &mut out);
        assert_eq!(n, 2);
        assert_eq!(out[0].event_type, 0x01);
        assert_eq!(out[0].addr, [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
        assert_eq!(out[0].data_len, 5);
        assert_eq!(&out[0].data[..5], &[0x02, 0x01, 0x06, 0x03, 0x19]);
        assert_eq!(out[0].rssi, -55);
        assert_eq!(out[1].addr_type, 0x01);
        assert_eq!(out[1].addr, [1, 2, 3, 4, 5, 6]);
        assert_eq!(out[1].data_len, 2);
        assert_eq!(out[1].rssi, -49);
        // 截断：报告 1 缺 rssi → 全部丢弃
        assert_eq!(parse_le_adv_reports(&params[..16], &mut out), 0);
        // 子事件不符
        params[0] = SUBEVT_CONN_COMPLETE;
        assert_eq!(parse_le_adv_reports(&params[..29], &mut out), 0);
        params[0] = SUBEVT_ADV_REPORT;
        // 只解析一条
        let n = parse_le_adv_reports(&params[..17], &mut out);
        assert_eq!(n, 1);
        assert_eq!(out[0].rssi, -55);
    }

    #[test]
    fn conn_complete_parsing() {
        let mut params = [0u8; 18];
        params[0] = SUBEVT_CONN_COMPLETE;
        params[1] = 0x00; // status OK
        params[2..4].copy_from_slice(&0x0042u16.to_le_bytes());
        params[4] = 0x01; // role: master
        params[5] = 0x00; // peer addr type: public
        params[6..12].copy_from_slice(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
        params[12..14].copy_from_slice(&0x0018u16.to_le_bytes());
        let cc = parse_le_conn_complete(&params[..14]).unwrap();
        assert_eq!(cc.status, 0);
        assert_eq!(cc.handle, 0x42);
        assert_eq!(cc.role, 1);
        assert_eq!(cc.peer_addr_type, 0);
        assert_eq!(cc.peer_addr, [0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
        assert_eq!(cc.interval, 0x18);
        // 截断 / 错误子事件
        assert!(parse_le_conn_complete(&params[..13]).is_none());
        params[0] = SUBEVT_ADV_REPORT;
        assert!(parse_le_conn_complete(&params[..14]).is_none());
    }

    #[test]
    fn command_builders() {
        let mut out = [0u8; 64];
        // Read BD_ADDR → 0x1009
        let n = cmd_read_bd_addr(&mut out).unwrap();
        assert_eq!(&out[..4], &[H4_CMD, 0x09, 0x10, 0x00]);
        assert_eq!(n, 4);
        // Read Local Version → 0x1001
        cmd_read_local_version(&mut out).unwrap();
        assert_eq!(&out[..4], &[H4_CMD, 0x01, 0x10, 0x00]);
        // Create Connection：25 字节参数，对端地址位于参数偏移 6（帧偏移 10）
        let addr = [0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F];
        let n = cmd_le_create_conn(1, &addr, &mut out).unwrap();
        assert_eq!(out[3], 25);
        assert_eq!(n, 4 + 25);
        assert_eq!(out[9], 1); // peer_addr_type（参数偏移 5）
        assert_eq!(&out[10..16], &addr);
        assert_eq!(u16::from_le_bytes([out[23], out[24]]), 0x02EE);
        // Adv Enable / Adv Data
        cmd_le_set_adv_enable(true, &mut out).unwrap();
        assert_eq!(&out[..5], &[H4_CMD, 0x0A, 0x20, 0x01, 0x01]);
        cmd_le_set_adv_data(&[0x02, 0x01, 0x06], &mut out).unwrap();
        assert_eq!(out[3], 4);
        assert_eq!(out[4], 3);
        assert_eq!(&out[5..8], &[0x02, 0x01, 0x06]);
        assert!(cmd_le_set_adv_data(&[0u8; 32], &mut out).is_none());
        // Scan Enable
        cmd_le_set_scan_enable(true, true, &mut out).unwrap();
        assert_eq!(&out[..6], &[H4_CMD, 0x0C, 0x20, 0x02, 0x01, 0x01]);
        // Connection Update：14 字节参数
        cmd_le_conn_update(0x42, 0x0006, 0x0006, &mut out).unwrap();
        assert_eq!(out[3], 14);
        assert_eq!(u16::from_le_bytes([out[4], out[5]]), 0x42);
        assert_eq!(u16::from_le_bytes([out[6], out[7]]), 6);
        assert_eq!(u16::from_le_bytes([out[8], out[9]]), 6);
        assert_eq!(u16::from_le_bytes([out[12], out[13]]), 0x02EE);
    }
}
