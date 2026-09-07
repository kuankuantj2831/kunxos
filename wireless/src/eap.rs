//! IEEE 802.1X EAP（Extensible Authentication Protocol，RFC 3748）。
//!
//! 提供：
//! - EAP 包编解码（Request / Response / Success / Failure）
//! - EAP-Identity
//! - EAP-MD5（RFC 3748 §7.1，完整实现）
//! - EAP-TLS（RFC 5216，流程骨架：握手状态机与消息编解码，证书链验证占位）
//! - EAPOL-Start / Logoff（802.1X 端口控制）

/// EAP 包 code。
pub const EAP_REQUEST: u8 = 1;
pub const EAP_RESPONSE: u8 = 2;
pub const EAP_SUCCESS: u8 = 3;
pub const EAP_FAILURE: u8 = 4;

/// EAP 类型。
pub const EAP_TYPE_IDENTITY: u8 = 1;
pub const EAP_TYPE_NOTIFICATION: u8 = 2;
pub const EAP_TYPE_MD5: u8 = 4;
pub const EAP_TYPE_TLS: u8 = 13;
pub const EAP_TYPE_TTLS: u8 = 21;
pub const EAP_TYPE_PEAP: u8 = 25;

/// EAP 包（固定 5 字节头 + 载荷）。
#[derive(Clone, Copy, Debug)]
pub struct EapPacket {
    pub code: u8,
    pub id: u8,
    pub etype: u8,
    pub data: [u8; 300],
    pub data_len: usize,
}

impl EapPacket {
    pub fn parse(frame: &[u8]) -> Option<Self> {
        if frame.len() < 5 {
            return None;
        }
        let code = frame[0];
        let id = frame[1];
        let len = u16::from_be_bytes([frame[2], frame[3]]) as usize;
        if len < 5 || len > frame.len() {
            return None;
        }
        let etype = frame[4];
        let mut data = [0u8; 300];
        let n = (len - 5).min(300);
        data[..n].copy_from_slice(&frame[5..5 + n]);
        Some(EapPacket { code, id, etype, data, data_len: n })
    }

    pub fn build(&self, out: &mut [u8]) -> Option<usize> {
        let total = 5 + self.data_len;
        if out.len() < total {
            return None;
        }
        out[0] = self.code;
        out[1] = self.id;
        out[2..4].copy_from_slice(&(total as u16).to_be_bytes());
        out[4] = self.etype;
        out[5..total].copy_from_slice(&self.data[..self.data_len]);
        Some(total)
    }
}

/// EAPOL 帧头类型。
pub const EAPOL_TYPE_EAP: u8 = 0;
pub const EAPOL_TYPE_START: u8 = 1;
pub const EAPOL_TYPE_LOGOFF: u8 = 2;
pub const EAPOL_TYPE_KEY: u8 = 3;

/// 构造 EAPOL-Start 帧（802.1X 端口接入发起）。
pub fn build_eapol_start(out: &mut [u8]) -> Option<usize> {
    if out.len() < 4 {
        return None;
    }
    out[0] = 2; // EAPOL version 2
    out[1] = EAPOL_TYPE_START;
    out[2] = 0;
    out[3] = 0;
    Some(4)
}

/// 构造 EAP Response/Identity。
pub fn build_identity_response(id: u8, identity: &[u8], out: &mut [u8]) -> Option<usize> {
    let pkt = EapPacket {
        code: EAP_RESPONSE,
        id,
        etype: EAP_TYPE_IDENTITY,
        data: [0u8; 300],
        data_len: identity.len().min(300),
    };
    let mut e = pkt;
    e.data[..e.data_len].copy_from_slice(&identity[..e.data_len]);
    e.build(out)
}

/// 解析 EAP-MD5 Request：返回挑战值（type-data 内：value-size(1) || value || name）。
pub fn parse_md5_request(data: &[u8]) -> Option<&[u8]> {
    if data.len() < 1 {
        return None;
    }
    let size = data[0] as usize;
    if 1 + size > data.len() {
        return None;
    }
    Some(&data[1..1 + size])
}

/// 构造 EAP-MD5 Response：value-size || MD5(id || password || challenge) || name。
pub fn build_md5_response(
    id: u8,
    password: &[u8],
    challenge: &[u8],
    name: &[u8],
    out: &mut [u8],
) -> Option<usize> {
    let digest = crate::md5::eap_md5_response(id, password, challenge);
    let mut data = [0u8; 300];
    data[0] = 16;
    data[1..17].copy_from_slice(&digest);
    let nlen = name.len().min(64);
    data[17..17 + nlen].copy_from_slice(&name[..nlen]);
    let pkt = EapPacket {
        code: EAP_RESPONSE,
        id,
        etype: EAP_TYPE_MD5,
        data,
        data_len: 17 + nlen,
    };
    pkt.build(out)
}

/// EAP 认证方状态机（对 EAP-Identity/MD5 的完整流程）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EapAuthState {
    Idle,
    SentIdentityReq,
    SentMd5Challenge,
    Authenticated,
    Failed,
}

/// EAP 认证方（authenticator）会话。
#[derive(Clone, Copy)]
pub struct EapAuthenticator {
    pub state: EapAuthState,
    pub id: u8,
    pub identity: [u8; 64],
    pub identity_len: usize,
    /// 预设密码（模拟认证服务器）。
    pub password: [u8; 64],
    pub password_len: usize,
    pub challenge: [u8; 16],
    pub challenge_len: usize,
}

impl EapAuthenticator {
    pub fn new(password: &[u8]) -> Self {
        let mut p = [0u8; 64];
        let n = password.len().min(64);
        p[..n].copy_from_slice(&password[..n]);
        EapAuthenticator {
            state: EapAuthState::Idle,
            id: 1,
            identity: [0u8; 64],
            identity_len: 0,
            password: p,
            password_len: n,
            challenge: [0u8; 16],
            challenge_len: 16,
        }
    }

    /// 处理收到的 EAP Response，生成下一条消息（EAP Request 或 Success/Failure）。
    /// 返回 (out_len, 是否终止)。
    pub fn handle(&mut self, resp: &EapPacket, out: &mut [u8]) -> (usize, bool) {
        match self.state {
            EapAuthState::Idle => {
                // 收到 Identity → 发 MD5 挑战
                self.identity[..resp.data_len.min(64)].copy_from_slice(&resp.data[..resp.data_len.min(64)]);
                self.identity_len = resp.data_len.min(64);
                self.state = EapAuthState::SentMd5Challenge;
                // 挑战值（确定性：id 与 identity 派生，模拟服务器行为）
                let mut ch = [0u8; 16];
                let mut seed = [0u8; 80];
                seed[..self.identity_len].copy_from_slice(&self.identity[..self.identity_len]);
                seed[self.identity_len] = self.id;
                let h = crate::sha2::sha256(&seed[..self.identity_len + 1]);
                ch.copy_from_slice(&h[..16]);
                self.challenge = ch;
                let mut data = [0u8; 300];
                data[0] = 16;
                data[1..17].copy_from_slice(&ch);
                let pkt = EapPacket {
                    code: EAP_REQUEST,
                    id: self.id,
                    etype: EAP_TYPE_MD5,
                    data,
                    data_len: 17,
                };
                let n = pkt.build(out).unwrap_or(0);
                (n, false)
            }
            EapAuthState::SentMd5Challenge => {
                // 校验 MD5 响应
                let challenge = &self.challenge[..self.challenge_len];
                let expected = crate::md5::eap_md5_response(self.id, &self.password[..self.password_len], challenge);
                let valid = resp.data_len >= 17 && resp.data[1..17] == expected;
                if valid {
                    self.state = EapAuthState::Authenticated;
                    let n = EapPacket {
                        code: EAP_SUCCESS,
                        id: self.id,
                        etype: 0,
                        data: [0u8; 300],
                        data_len: 0,
                    }
                    .build(out)
                    .unwrap_or(0);
                    (n, true)
                } else {
                    self.state = EapAuthState::Failed;
                    let n = EapPacket {
                        code: EAP_FAILURE,
                        id: self.id,
                        etype: 0,
                        data: [0u8; 300],
                        data_len: 0,
                    }
                    .build(out)
                    .unwrap_or(0);
                    (n, true)
                }
            }
            _ => (0, true),
        }
    }
}

/// EAP-TLS 流程骨架状态（RFC 5216）：消息类型在 TLS 记录内。
/// 完整 TLS 握手不在本次范围；此处提供流程状态机与占位，供 802.1X 集成。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EapTlsPhase {
    Start,          // 等待 EAP-Request/TLS Start（type-data 首位 0x20）
    ClientHello,    // 已发 ClientHello
    ServerHello,    // 已收 ServerHello（证书链验证占位）
    Done,
}

/// EAP-TLS 会话（骨架）。
#[derive(Clone, Copy)]
pub struct EapTlsSession {
    pub phase: EapTlsPhase,
    pub id: u8,
}

impl EapTlsSession {
    pub fn new(id: u8) -> Self {
        EapTlsSession { phase: EapTlsPhase::Start, id }
    }

    /// 解析 EAP-TLS 标志字节（type-data 首位）。
    pub fn flags(data: &[u8]) -> Option<u8> {
        data.first().copied()
    }

    /// 处理一条 EAP-TLS 消息，推进状态机。
    /// 返回下一条要发送的 EAP-Response/TLS 载荷（或 None 表示流程完成/失败）。
    pub fn handle(&mut self, req: &EapPacket, out: &mut [u8]) -> (usize, bool) {
        let _ = req;
        match self.phase {
            EapTlsPhase::Start => {
                // 收到 TLS Start：发 ClientHello（骨架：空握手占位）
                self.phase = EapTlsPhase::ClientHello;
                let mut data = [0u8; 300];
                data[0] = 0x20; // EAP-TLS flags：无分片、无附加
                let pkt = EapPacket {
                    code: EAP_RESPONSE,
                    id: self.id,
                    etype: EAP_TYPE_TLS,
                    data,
                    data_len: 1,
                };
                let n = pkt.build(out).unwrap_or(0);
                (n, false)
            }
            EapTlsPhase::ClientHello => {
                // 已收 ServerHello 片段（占位：不解析 TLS 记录细节）
                self.phase = EapTlsPhase::ServerHello;
                (0, false)
            }
            EapTlsPhase::ServerHello => {
                // 流程结束（证书验证 / ChangeCipherSpec / Finished 为后续工作）
                self.phase = EapTlsPhase::Done;
                (0, true)
            }
            EapTlsPhase::Done => (0, true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eap_md5_full_flow() {
        let mut auth = EapAuthenticator::new(b"secret123");
        // STA：发 Identity
        let mut resp = [0u8; 128];
        let n = build_identity_response(1, b"user@kunxos", &mut resp).unwrap();
        let pkt = EapPacket::parse(&resp[..n]).unwrap();
        assert_eq!(pkt.code, EAP_RESPONSE);
        assert_eq!(pkt.etype, EAP_TYPE_IDENTITY);

        // 认证方 → MD5 挑战
        let mut req = [0u8; 128];
        let (n, done) = auth.handle(&pkt, &mut req);
        assert!(!done);
        assert!(n > 0);
        let req_pkt = EapPacket::parse(&req[..n]).unwrap();
        assert_eq!(req_pkt.code, EAP_REQUEST);
        assert_eq!(req_pkt.etype, EAP_TYPE_MD5);
        let challenge = parse_md5_request(&req_pkt.data[..req_pkt.data_len]).unwrap().to_vec();

        // STA：发 MD5 响应（用正确密码）
        let mut resp2 = [0u8; 128];
        let n2 = build_md5_response(req_pkt.id, b"secret123", &challenge, b"", &mut resp2).unwrap();
        let pkt2 = EapPacket::parse(&resp2[..n2]).unwrap();
        let (n3, done3) = auth.handle(&pkt2, &mut resp2);
        assert!(done3);
        let final_pkt = EapPacket::parse(&resp2[..n3]).unwrap();
        assert_eq!(final_pkt.code, EAP_SUCCESS);

        // 错误密码 → Failure
        let mut auth2 = EapAuthenticator::new(b"secret123");
        let mut r1 = [0u8; 128];
        let n = build_identity_response(1, b"user", &mut r1).unwrap();
        let p1 = EapPacket::parse(&r1[..n]).unwrap();
        let mut r2 = [0u8; 128];
        let (n2, _) = auth2.handle(&p1, &mut r2);
        let p2 = EapPacket::parse(&r2[..n2]).unwrap();
        let ch = parse_md5_request(&p2.data[..p2.data_len]).unwrap().to_vec();
        let mut r3 = [0u8; 128];
        let n3 = build_md5_response(p2.id, b"WRONG", &ch, b"", &mut r3).unwrap();
        let p3 = EapPacket::parse(&r3[..n3]).unwrap();
        let (n4, done4) = auth2.handle(&p3, &mut r3);
        assert!(done4);
        assert_eq!(EapPacket::parse(&r3[..n4]).unwrap().code, EAP_FAILURE);
    }

    #[test]
    fn eap_packet_roundtrip() {
        let pkt = EapPacket {
            code: EAP_RESPONSE,
            id: 7,
            etype: EAP_TYPE_IDENTITY,
            data: [0u8; 300],
            data_len: 5,
        };
        let mut buf = [0u8; 64];
        let n = pkt.build(&mut buf).unwrap();
        let back = EapPacket::parse(&buf[..n]).unwrap();
        assert_eq!(back.code, EAP_RESPONSE);
        assert_eq!(back.id, 7);
        assert_eq!(back.etype, EAP_TYPE_IDENTITY);
    }
}
