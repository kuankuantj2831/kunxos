//! ATT（属性协议）：PDU 编解码 + 最小服务端引擎。
//!
//! PDU 布局：`[opcode(1), payload...]`；多字节字段一律小端。
//! 所有函数均为纯函数，越界返回 `None`，绝不 panic；服务端引擎线性扫描固定属性表
//! （无堆），把请求 PDU 翻译为标准响应 PDU。

/// ATT 操作码。
pub const OP_ERROR_RESPONSE: u8 = 0x01;
pub const OP_EXCHANGE_MTU_REQ: u8 = 0x02;
pub const OP_EXCHANGE_MTU_RSP: u8 = 0x03;
pub const OP_FIND_INFORMATION_REQ: u8 = 0x04;
pub const OP_FIND_INFORMATION_RSP: u8 = 0x05;
pub const OP_READ_BY_TYPE_REQ: u8 = 0x08;
pub const OP_READ_BY_TYPE_RSP: u8 = 0x09;
pub const OP_READ_REQ: u8 = 0x0A;
pub const OP_READ_RSP: u8 = 0x0B;
pub const OP_READ_BLOB_REQ: u8 = 0x0C;
pub const OP_READ_BLOB_RSP: u8 = 0x0D;
pub const OP_WRITE_REQ: u8 = 0x12;
pub const OP_WRITE_RSP: u8 = 0x13;
pub const OP_WRITE_CMD: u8 = 0x52;
pub const OP_HANDLE_VALUE_NOTIFY: u8 = 0x1B;
pub const OP_HANDLE_VALUE_INDICATE: u8 = 0x1D;
pub const OP_HANDLE_VALUE_CONFIRM: u8 = 0x1E;
pub const OP_PREPARE_WRITE_REQ: u8 = 0x16;
pub const OP_EXECUTE_WRITE_REQ: u8 = 0x18;

/// 错误码（Error Response 的 error 字段）。
pub const ERR_INVALID_HANDLE: u8 = 0x01;
pub const ERR_READ_NOT_PERMITTED: u8 = 0x02;
pub const ERR_WRITE_NOT_PERMITTED: u8 = 0x03;
pub const ERR_INVALID_PDU: u8 = 0x04;
pub const ERR_REQUEST_NOT_SUPPORTED: u8 = 0x06;
pub const ERR_INVALID_OFFSET: u8 = 0x07;
pub const ERR_ATTRIBUTE_NOT_FOUND: u8 = 0x0A;
pub const ERR_INVALID_ATTRIBUTE_LENGTH: u8 = 0x0D;

/// ATT 包：opcode + 载荷（固定最大 256 字节缓冲）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AttPacket {
    pub opcode: u8,
    pub data: [u8; 256],
    pub len: usize,
}

/// 解析一个完整 ATT PDU（首字节为 opcode）。
/// 空帧或载荷超过 256 字节返回 `None`。
pub fn parse_att(frame: &[u8]) -> Option<AttPacket> {
    if frame.is_empty() {
        return None;
    }
    let plen = frame.len() - 1;
    if plen > 256 {
        return None;
    }
    let mut data = [0u8; 256];
    data[..plen].copy_from_slice(&frame[1..]);
    Some(AttPacket {
        opcode: frame[0],
        data,
        len: plen,
    })
}

/// 构造 ATT PDU：`[opcode, payload...]`，写入 out，返回长度。
/// 载荷超过 256 字节或 `out` 空间不足返回 `None`。
pub fn build_att(opcode: u8, payload: &[u8], out: &mut [u8]) -> Option<usize> {
    if payload.len() > 256 {
        return None;
    }
    let n = 1 + payload.len();
    if out.len() < n {
        return None;
    }
    out[0] = opcode;
    out[1..n].copy_from_slice(payload);
    Some(n)
}

/// 构造 Error Response：`[0x01, req_opcode, handle(2 LE), error]`。
pub fn build_error_response(req_opcode: u8, handle: u16, err: u8, out: &mut [u8]) -> Option<usize> {
    if out.len() < 5 {
        return None;
    }
    out[0] = OP_ERROR_RESPONSE;
    out[1] = req_opcode;
    out[2..4].copy_from_slice(&handle.to_le_bytes());
    out[4] = err;
    Some(5)
}

/// 解析 Read By Type Req：返回 `(start_handle, end_handle, uuid, uuid_len)`。
///
/// 载荷：`[start(2 LE), end(2 LE), uuid(2 或 16)]`。16 位 UUID 在 ATT PDU 中按小端
/// 传输，原样存入 `uuid[0..2]`（调用方用 `u16::from_le_bytes` 还原）。
pub fn parse_read_by_type_req(payload: &[u8]) -> Option<(u16, u16, [u8; 16], usize)> {
    if payload.len() < 6 {
        return None;
    }
    let start = u16::from_le_bytes([payload[0], payload[1]]);
    let end = u16::from_le_bytes([payload[2], payload[3]]);
    let ulen = payload.len() - 4;
    if ulen != 2 && ulen != 16 {
        return None;
    }
    let mut uuid = [0u8; 16];
    uuid[..ulen].copy_from_slice(&payload[4..]);
    Some((start, end, uuid, ulen))
}

/// 解析 Find Information Req：返回 `(start, end)`。
pub fn parse_find_info_req(payload: &[u8]) -> Option<(u16, u16)> {
    if payload.len() < 4 {
        return None;
    }
    Some((
        u16::from_le_bytes([payload[0], payload[1]]),
        u16::from_le_bytes([payload[2], payload[3]]),
    ))
}

/// 解析 Write Req / Write Cmd：返回 `(handle, value)`。
pub fn parse_write(payload: &[u8]) -> Option<(u16, &[u8])> {
    if payload.len() < 2 {
        return None;
    }
    Some((u16::from_le_bytes([payload[0], payload[1]]), &payload[2..]))
}

/// 解析 Exchange MTU Req：返回客户端 MTU。
pub fn parse_exchange_mtu_req(payload: &[u8]) -> Option<u16> {
    if payload.len() < 2 {
        return None;
    }
    Some(u16::from_le_bytes([payload[0], payload[1]]))
}

/// ATT 属性表项：固定 32 字节值缓冲（无堆）。
#[derive(Clone, Copy, Debug)]
pub struct AttAttr {
    pub handle: u16,
    pub uuid16: u16,
    pub readable: bool,
    pub writable: bool,
    pub value: [u8; 32],
    pub value_len: usize,
}

/// 有效值长度：对 `value_len` 越界（> 32）做防御性钳制，杜绝库内 panic。
fn vlen(a: &AttAttr) -> usize {
    a.value_len.min(a.value.len())
}

/// ATT 服务端最小引擎：线性扫描属性表，把请求 PDU 翻译为标准响应 PDU。
///
/// 支持：Exchange MTU / Find Information / Read By Type（16 位 UUID）/
/// Read / Read Blob / Write Req / Write Cmd；Prepare Write 按不支持处理。
/// `handle` 返回 `Some(0)` 表示该 PDU 无需响应（Write Command / Confirm）。
pub struct AttServer<'a> {
    pub attrs: &'a mut [AttAttr],
    /// 协商后的 ATT_MTU（默认 23）。
    pub mtu: u16,
}

impl<'a> AttServer<'a> {
    pub fn new(attrs: &'a mut [AttAttr]) -> Self {
        AttServer { attrs, mtu: 23 }
    }

    /// 处理一个请求 PDU，把响应写入 out，返回响应长度。
    /// 空 PDU 返回 `None`。
    pub fn handle(&mut self, pdu: &[u8], out: &mut [u8]) -> Option<usize> {
        if pdu.is_empty() {
            return None;
        }
        match pdu[0] {
            OP_EXCHANGE_MTU_REQ => self.on_mtu(pdu, out),
            OP_FIND_INFORMATION_REQ => self.on_find_info(pdu, out),
            OP_READ_BY_TYPE_REQ => self.on_read_by_type(pdu, out),
            OP_READ_REQ => self.on_read(pdu, out),
            OP_READ_BLOB_REQ => self.on_read_blob(pdu, out),
            OP_WRITE_REQ => self.on_write(pdu, out, true),
            OP_WRITE_CMD => self.on_write(pdu, out, false),
            // 无 prepared write 时，Execute Write 直接成功（空写）。
            OP_EXECUTE_WRITE_REQ => build_att(OP_WRITE_RSP, &[], out),
            OP_HANDLE_VALUE_CONFIRM => Some(0),
            OP_PREPARE_WRITE_REQ => {
                build_error_response(OP_PREPARE_WRITE_REQ, 0, ERR_WRITE_NOT_PERMITTED, out)
            }
            _ => build_error_response(pdu[0], 0, ERR_REQUEST_NOT_SUPPORTED, out),
        }
    }

    fn find_index(&self, handle: u16) -> Option<usize> {
        self.attrs.iter().position(|a| a.handle == handle)
    }

    fn on_mtu(&mut self, pdu: &[u8], out: &mut [u8]) -> Option<usize> {
        let client = match parse_exchange_mtu_req(&pdu[1..]) {
            Some(m) => m,
            None => return build_error_response(OP_EXCHANGE_MTU_REQ, 0, ERR_INVALID_PDU, out),
        };
        // 本方 RX MTU 上限 256（缓冲上限）；协商值取 min，下限 23。
        let negotiated = client.min(256).max(23);
        self.mtu = negotiated;
        build_att(OP_EXCHANGE_MTU_RSP, &256u16.to_le_bytes(), out)
    }

    fn on_find_info(&mut self, pdu: &[u8], out: &mut [u8]) -> Option<usize> {
        let (start, end) = match parse_find_info_req(&pdu[1..]) {
            Some(v) => v,
            None => return build_error_response(OP_FIND_INFORMATION_REQ, 0, ERR_INVALID_PDU, out),
        };
        if out.len() < 2 {
            return None;
        }
        out[0] = OP_FIND_INFORMATION_RSP;
        out[1] = 0x01; // format 1：16 位 UUID 对
        let mut pos = 2;
        for a in self.attrs.iter() {
            if a.handle < start || a.handle > end || a.uuid16 == 0 {
                continue;
            }
            if pos + 4 > out.len() {
                break;
            }
            out[pos..pos + 2].copy_from_slice(&a.handle.to_le_bytes());
            out[pos + 2..pos + 4].copy_from_slice(&a.uuid16.to_le_bytes());
            pos += 4;
        }
        if pos == 2 {
            return build_error_response(OP_FIND_INFORMATION_REQ, start, ERR_ATTRIBUTE_NOT_FOUND, out);
        }
        Some(pos)
    }

    fn on_read_by_type(&mut self, pdu: &[u8], out: &mut [u8]) -> Option<usize> {
        let (start, end, uuid, ulen) = match parse_read_by_type_req(&pdu[1..]) {
            Some(v) => v,
            None => return build_error_response(OP_READ_BY_TYPE_REQ, 0, ERR_INVALID_PDU, out),
        };
        if ulen != 2 {
            // 属性表只有 16 位 UUID；128 位查询一律找不到
            return build_error_response(OP_READ_BY_TYPE_REQ, start, ERR_ATTRIBUTE_NOT_FOUND, out);
        }
        let want = u16::from_le_bytes([uuid[0], uuid[1]]);
        // 先确定首个匹配项的条目长度（handle 2 + value）
        let mut item_len = 0usize;
        for a in self.attrs.iter() {
            if a.handle >= start && a.handle <= end && a.uuid16 == want {
                item_len = 2 + vlen(a);
                break;
            }
        }
        if item_len == 0 {
            return build_error_response(OP_READ_BY_TYPE_REQ, start, ERR_ATTRIBUTE_NOT_FOUND, out);
        }
        if out.len() < 2 + item_len {
            return None;
        }
        out[0] = OP_READ_BY_TYPE_RSP;
        out[1] = item_len as u8;
        let mut pos = 2;
        for a in self.attrs.iter() {
            if a.handle < start || a.handle > end || a.uuid16 != want {
                continue;
            }
            if 2 + vlen(a) != item_len {
                continue; // 规范要求同一条目长度
            }
            if pos + item_len > out.len() {
                break;
            }
            out[pos..pos + 2].copy_from_slice(&a.handle.to_le_bytes());
            let vl = vlen(a);
            out[pos + 2..pos + 2 + vl].copy_from_slice(&a.value[..vl]);
            pos += item_len;
        }
        Some(pos)
    }

    fn on_read(&mut self, pdu: &[u8], out: &mut [u8]) -> Option<usize> {
        if pdu.len() < 3 {
            return build_error_response(OP_READ_REQ, 0, ERR_INVALID_PDU, out);
        }
        let handle = u16::from_le_bytes([pdu[1], pdu[2]]);
        let a = match self.attrs.iter().find(|a| a.handle == handle) {
            Some(a) => a,
            None => return build_error_response(OP_READ_REQ, handle, ERR_INVALID_HANDLE, out),
        };
        if !a.readable {
            return build_error_response(OP_READ_REQ, handle, ERR_READ_NOT_PERMITTED, out);
        }
        let vl = vlen(a);
        build_att(OP_READ_RSP, &a.value[..vl], out)
    }

    fn on_read_blob(&mut self, pdu: &[u8], out: &mut [u8]) -> Option<usize> {
        if pdu.len() < 5 {
            return build_error_response(OP_READ_BLOB_REQ, 0, ERR_INVALID_PDU, out);
        }
        let handle = u16::from_le_bytes([pdu[1], pdu[2]]);
        let offset = u16::from_le_bytes([pdu[3], pdu[4]]) as usize;
        let a = match self.attrs.iter().find(|a| a.handle == handle) {
            Some(a) => a,
            None => return build_error_response(OP_READ_BLOB_REQ, handle, ERR_INVALID_HANDLE, out),
        };
        if !a.readable {
            return build_error_response(OP_READ_BLOB_REQ, handle, ERR_READ_NOT_PERMITTED, out);
        }
        let vl = vlen(a);
        if offset >= vl {
            return build_error_response(OP_READ_BLOB_REQ, handle, ERR_INVALID_OFFSET, out);
        }
        build_att(OP_READ_BLOB_RSP, &a.value[offset..vl], out)
    }

    fn on_write(&mut self, pdu: &[u8], out: &mut [u8], respond: bool) -> Option<usize> {
        let opcode = if respond { OP_WRITE_REQ } else { OP_WRITE_CMD };
        if pdu.len() < 3 {
            return build_error_response(opcode, 0, ERR_INVALID_PDU, out);
        }
        let handle = u16::from_le_bytes([pdu[1], pdu[2]]);
        let value = &pdu[3..];
        let idx = match self.find_index(handle) {
            Some(i) => i,
            None => return build_error_response(opcode, handle, ERR_INVALID_HANDLE, out),
        };
        {
            let a = &self.attrs[idx];
            if !a.writable {
                return build_error_response(opcode, handle, ERR_WRITE_NOT_PERMITTED, out);
            }
            if value.len() > a.value.len() {
                return build_error_response(opcode, handle, ERR_INVALID_ATTRIBUTE_LENGTH, out);
            }
        }
        let a = &mut self.attrs[idx];
        a.value[..value.len()].copy_from_slice(value);
        a.value_len = value.len();
        if respond {
            build_att(OP_WRITE_RSP, &[], out)
        } else {
            Some(0) // Write Command 无响应
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attr(handle: u16, uuid16: u16, readable: bool, writable: bool, value: &[u8]) -> AttAttr {
        let mut a = AttAttr {
            handle,
            uuid16,
            readable,
            writable,
            value: [0u8; 32],
            value_len: value.len(),
        };
        a.value[..value.len()].copy_from_slice(value);
        a
    }

    #[test]
    fn att_packet_roundtrip() {
        let payload = [0x11, 0x22, 0x33, 0x44];
        let mut out = [0u8; 64];
        let n = build_att(OP_WRITE_REQ, &payload, &mut out).unwrap();
        assert_eq!(n, 5);
        let p = parse_att(&out[..n]).unwrap();
        assert_eq!(p.opcode, OP_WRITE_REQ);
        assert_eq!(p.len, 4);
        assert_eq!(&p.data[..4], &payload);
        // 空载荷
        let n = build_att(OP_WRITE_RSP, &[], &mut out).unwrap();
        let p = parse_att(&out[..n]).unwrap();
        assert_eq!((p.opcode, p.len), (OP_WRITE_RSP, 0));
        // 空帧 / 超长
        assert!(parse_att(&[]).is_none());
        assert!(build_att(OP_WRITE_REQ, &[0u8; 257], &mut out).is_none());
    }

    #[test]
    fn request_parsers() {
        // Read By Type：16 位 UUID
        let req = [0x01, 0x00, 0xFF, 0xFF, 0x03, 0x28];
        let (s, e, uuid, ulen) = parse_read_by_type_req(&req).unwrap();
        assert_eq!((s, e, ulen), (1, 0xFFFF, 2));
        assert_eq!(u16::from_le_bytes([uuid[0], uuid[1]]), 0x2803);
        // Read By Type：128 位 UUID
        let mut req128 = [0u8; 20];
        req128[..4].copy_from_slice(&[0x00, 0x00, 0xFF, 0xFF]);
        for i in 0..16 {
            req128[4 + i] = i as u8;
        }
        let (_, _, uuid, ulen2) = parse_read_by_type_req(&req128).unwrap();
        assert_eq!(ulen2, 16);
        assert_eq!(uuid[0], 0);
        assert_eq!(uuid[15], 15);
        assert!(parse_read_by_type_req(&[0u8; 5]).is_none());
        assert!(parse_read_by_type_req(&[0u8; 7]).is_none()); // uuid 长度非法
        // Write
        let wr = [0x03, 0x00, 0x41, 0x42, 0x43];
        let (h, v) = parse_write(&wr).unwrap();
        assert_eq!(h, 3);
        assert_eq!(v, &[0x41, 0x42, 0x43]);
        assert!(parse_write(&[0x00]).is_none());
        // MTU
        assert_eq!(parse_exchange_mtu_req(&[0x40, 0x00]), Some(64));
        assert_eq!(parse_exchange_mtu_req(&[0x00]), None);
        // Find Info
        assert_eq!(parse_find_info_req(&[0x10, 0x00, 0x20, 0x00]), Some((16, 32)));
    }

    #[test]
    fn error_response_format() {
        let mut out = [0u8; 16];
        let n = build_error_response(OP_READ_REQ, 0x0007, ERR_INVALID_HANDLE, &mut out).unwrap();
        assert_eq!(n, 5);
        assert_eq!(
            &out[..n],
            &[OP_ERROR_RESPONSE, OP_READ_REQ, 0x07, 0x00, ERR_INVALID_HANDLE]
        );
        // 空间不足
        assert!(build_error_response(OP_READ_REQ, 7, ERR_INVALID_HANDLE, &mut [0u8; 4]).is_none());
    }

    #[test]
    fn server_read_write_roundtrip() {
        let mut attrs = [
            attr(1, 0x2800, true, false, &[0x00, 0x18]), // GAP 服务声明
            attr(2, 0x2A00, true, true, b""), // 设备名（初始为空）
            attr(3, 0x2902, true, true, &[0x00, 0x00]), // CCCD
        ];
        let mut server = AttServer::new(&mut attrs);
        let mut req = [0u8; 64];
        let mut rsp = [0u8; 128];
        // 写设备名
        let n = build_att(OP_WRITE_REQ, &[2, 0, b'K', b'u', b'n', b'x'], &mut req).unwrap();
        server.handle(&req[..n], &mut rsp).unwrap();
        assert_eq!(rsp[0], OP_WRITE_RSP);
        // 读回
        let n = build_att(OP_READ_REQ, &[2, 0], &mut req).unwrap();
        let rn = server.handle(&req[..n], &mut rsp).unwrap();
        assert_eq!(rsp[0], OP_READ_RSP);
        assert_eq!(&rsp[1..rn], b"Kunx");
        // 写 Command（无响应）
        let n = build_att(OP_WRITE_CMD, &[2, 0, b'X'], &mut req).unwrap();
        assert_eq!(server.handle(&req[..n], &mut rsp).unwrap(), 0);
        let n = build_att(OP_READ_REQ, &[2, 0], &mut req).unwrap();
        let rn = server.handle(&req[..n], &mut rsp).unwrap();
        assert_eq!(&rsp[1..rn], b"X");
        // 读不可写属性（handle 1 可读但不可写 → 写被拒）
        let n = build_att(OP_WRITE_REQ, &[1, 0, 0xAA], &mut req).unwrap();
        server.handle(&req[..n], &mut rsp).unwrap();
        assert_eq!(rsp[0], OP_ERROR_RESPONSE);
        assert_eq!(rsp[4], ERR_WRITE_NOT_PERMITTED);
        // 未知句柄
        let n = build_att(OP_READ_REQ, &[0x63, 0x00], &mut req).unwrap();
        server.handle(&req[..n], &mut rsp).unwrap();
        assert_eq!(rsp[0], OP_ERROR_RESPONSE);
        assert_eq!(rsp[4], ERR_INVALID_HANDLE);
    }

    #[test]
    fn server_mtu_discovery_and_errors() {
        let mut attrs = [
            attr(1, 0x2800, true, false, &[0x00, 0x18]),
            attr(2, 0x2803, true, false, &[0x02, 0x03, 0x00, 0x00, 0x2A]),
            attr(3, 0x2A00, true, false, b"Kunx"),
            attr(4, 0x2902, true, true, &[0x00, 0x00]),
        ];
        let mut server = AttServer::new(&mut attrs);
        let mut req = [0u8; 64];
        let mut rsp = [0u8; 128];
        // MTU 交换：客户端 320 → 协商 256，响应广告本方 RX MTU 256
        let n = build_att(OP_EXCHANGE_MTU_REQ, &[0x40, 0x01], &mut req).unwrap();
        server.handle(&req[..n], &mut rsp).unwrap();
        assert_eq!(rsp[0], OP_EXCHANGE_MTU_RSP);
        assert_eq!(u16::from_le_bytes([rsp[1], rsp[2]]), 256);
        assert_eq!(server.mtu, 256);
        // Read By Type 0x2803 → 一条 5 字节声明
        let n = build_att(OP_READ_BY_TYPE_REQ, &[1, 0, 4, 0, 0x03, 0x28], &mut req).unwrap();
        let rn = server.handle(&req[..n], &mut rsp).unwrap();
        assert_eq!(
            &rsp[..rn],
            &[OP_READ_BY_TYPE_RSP, 7, 2, 0, 0x02, 0x03, 0x00, 0x00, 0x2A]
        );
        // Read By Type 0x2800 → 一条 2 字节值
        let n = build_att(OP_READ_BY_TYPE_REQ, &[1, 0, 4, 0, 0x00, 0x28], &mut req).unwrap();
        let rn = server.handle(&req[..n], &mut rsp).unwrap();
        assert_eq!(&rsp[..rn], &[OP_READ_BY_TYPE_RSP, 4, 1, 0, 0x00, 0x18]);
        // 找不到 UUID → Attribute Not Found
        let n = build_att(OP_READ_BY_TYPE_REQ, &[1, 0, 4, 0, 0x99, 0x99], &mut req).unwrap();
        server.handle(&req[..n], &mut rsp).unwrap();
        assert_eq!(rsp[0], OP_ERROR_RESPONSE);
        assert_eq!(rsp[4], ERR_ATTRIBUTE_NOT_FOUND);
        // Find Information → 16 位对
        let n = build_att(OP_FIND_INFORMATION_REQ, &[1, 0, 4, 0], &mut req).unwrap();
        let rn = server.handle(&req[..n], &mut rsp).unwrap();
        assert_eq!(rsp[0], OP_FIND_INFORMATION_RSP);
        assert_eq!(rsp[1], 1);
        assert_eq!(rn, 2 + 4 * 4);
        assert_eq!(
            &rsp[2..rn],
            &[
                1, 0, 0x00, 0x28, // handle 1, uuid 0x2800
                2, 0, 0x03, 0x28, // handle 2, uuid 0x2803
                3, 0, 0x00, 0x2A, // handle 3, uuid 0x2A00
                4, 0, 0x02, 0x29, // handle 4, uuid 0x2902
            ]
        );
        // Read Blob：offset=2 → "nx"
        let n = build_att(OP_READ_BLOB_REQ, &[3, 0, 2, 0], &mut req).unwrap();
        let rn = server.handle(&req[..n], &mut rsp).unwrap();
        assert_eq!(rsp[0], OP_READ_BLOB_RSP);
        assert_eq!(&rsp[1..rn], b"nx");
        // Read Blob：offset 越界 → Invalid Offset
        let n = build_att(OP_READ_BLOB_REQ, &[3, 0, 0xFF, 0x00], &mut req).unwrap();
        server.handle(&req[..n], &mut rsp).unwrap();
        assert_eq!(rsp[0], OP_ERROR_RESPONSE);
        assert_eq!(rsp[4], ERR_INVALID_OFFSET);
        // 未知操作码 → Request Not Supported
        let n = build_att(0x77, &[], &mut req).unwrap();
        server.handle(&req[..n], &mut rsp).unwrap();
        assert_eq!(rsp[0], OP_ERROR_RESPONSE);
        assert_eq!(rsp[4], ERR_REQUEST_NOT_SUPPORTED);
    }
}
