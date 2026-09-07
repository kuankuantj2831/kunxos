//! A2DP（Advanced Audio Distribution Profile）：AVDTP 信令 + SBC 编解码器配置 + 媒体载荷。
//!
//! 覆盖：
//! - AVDTP 消息头（packet type / transaction label / message type）与信令标识符
//! - 信令消息：Discover、Get Capabilities、Set Configuration、Open、Start、
//!   Close、Suspend、Abort（构造与接受/拒绝响应）
//! - 服务能力（Service Capabilities）：媒体传输（0x01）、延迟上报（0x08）、
//!   媒体编解码器（0x07）
//! - SBC 编解码器能力（采样频率/通道模式/块长/子带/分配方法/位池）编解码
//! - 媒体载荷：RTP 头（V=2、PT=96）+ SBC 帧头（0x9C），含延迟相关字段
//!
//! AVDTP 运行于 L2CAP PSM 0x0019（信令）与静态通道（媒体）。纯函数、无堆。

/// AVDTP 信令 PSM。
pub const PSM_AVDTP: u16 = 0x0019;

// ==================== AVDTP 消息头 ====================

/// 包类型。
pub const PKT_SINGLE: u8 = 0;
pub const PKT_START: u8 = 1;
pub const PKT_CONTINUE: u8 = 2;
pub const PKT_END: u8 = 3;

/// 消息类型。
pub const MSG_CMD: u8 = 0;
pub const MSG_GENERAL_REJECT: u8 = 1;
pub const MSG_RESPONSE_ACCEPT: u8 = 2;
pub const MSG_RESPONSE_REJECT: u8 = 3;

/// 信令标识符。
pub const SIG_MEDIA: u8 = 0x00;
pub const SIG_DISCOVER: u8 = 0x01;
pub const SIG_GET_CAPABILITIES: u8 = 0x02;
pub const SIG_SET_CONFIGURATION: u8 = 0x03;
pub const SIG_GET_CONFIGURATION: u8 = 0x04;
pub const SIG_RECONFIGURE: u8 = 0x05;
pub const SIG_OPEN: u8 = 0x06;
pub const SIG_START: u8 = 0x07;
pub const SIG_CLOSE: u8 = 0x08;
pub const SIG_SUSPEND: u8 = 0x09;
pub const SIG_ABORT: u8 = 0x0A;
pub const SIG_SECURITY_CONTROL: u8 = 0x0B;

/// AVDTP 消息头。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AvdtpHeader {
    pub packet_type: u8,
    pub transaction: u8,
    pub message_type: u8,
    pub signal: u8,
}

/// 编码头（octet0 + octet1）。`octet0 = pkt_type<<6 | tl<<2 | msg_type`。
pub fn build_header(h: &AvdtpHeader, out: &mut [u8]) -> Option<usize> {
    if out.len() < 2 {
        return None;
    }
    out[0] = ((h.packet_type & 3) << 6) | ((h.transaction & 0x0F) << 2) | (h.message_type & 3);
    out[1] = h.signal;
    Some(2)
}

/// 解析头（至少 2 字节）。
pub fn parse_header(frame: &[u8]) -> Option<(AvdtpHeader, &[u8])> {
    if frame.len() < 2 {
        return None;
    }
    let o0 = frame[0];
    let h = AvdtpHeader {
        packet_type: o0 >> 6,
        transaction: (o0 >> 2) & 0x0F,
        message_type: o0 & 3,
        signal: frame[1],
    };
    Some((h, &frame[2..]))
}

// ==================== 服务能力 ====================

/// 服务类别。
pub const CAT_MEDIA_TRANSPORT: u8 = 0x01;
pub const CAT_REPORTING: u8 = 0x02;
pub const CAT_RECOVERY: u8 = 0x03;
pub const CAT_CONTENT_PROTECTION: u8 = 0x04;
pub const CAT_HEADER_COMPRESSION: u8 = 0x05;
pub const CAT_MULTIPLEXING: u8 = 0x06;
pub const CAT_MEDIA_CODEC: u8 = 0x07;
pub const CAT_DELAY_REPORTING: u8 = 0x08;

/// 媒体类型 / 编解码器类型（Media Codec 能力内）。
pub const MEDIA_TYPE_AUDIO: u8 = 0x00;
pub const CODEC_SBC: u8 = 0x00;

/// 媒体传输能力（无载荷）。
pub fn build_media_transport(out: &mut [u8]) -> Option<usize> {
    if out.len() < 2 {
        return None;
    }
    out[0] = CAT_MEDIA_TRANSPORT;
    out[1] = 0;
    Some(2)
}

/// 延迟上报能力（无载荷）。
pub fn build_delay_reporting(out: &mut [u8]) -> Option<usize> {
    if out.len() < 2 {
        return None;
    }
    out[0] = CAT_DELAY_REPORTING;
    out[1] = 0;
    Some(2)
}

// ==================== SBC 能力 ====================

/// SBC 采样频率/通道模式/块长/子带/分配位。
pub const SBC_SF_16K: u8 = 0;
pub const SBC_SF_32K: u8 = 1;
pub const SBC_SF_44K1: u8 = 2;
pub const SBC_SF_48K: u8 = 3;
pub const SBC_CH_MONO: u8 = 0;
pub const SBC_CH_DUAL: u8 = 1;
pub const SBC_CH_STEREO: u8 = 2;
pub const SBC_CH_JOINT: u8 = 3;
pub const SBC_BLOCK_4: u8 = 0;
pub const SBC_BLOCK_8: u8 = 1;
pub const SBC_BLOCK_12: u8 = 2;
pub const SBC_BLOCK_16: u8 = 3;
pub const SBC_SUBBANDS_4: u8 = 0;
pub const SBC_SUBBANDS_8: u8 = 1;
pub const SBC_ALLOC_LOUDNESS: u8 = 0;
pub const SBC_ALLOC_SNR: u8 = 1;

/// SBC 编解码器能力块。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SbcCaps {
    pub sampling_freq: u8,
    pub channel_mode: u8,
    pub block_len: u8,
    pub subbands: u8,
    pub alloc: u8,
    pub min_bitpool: u8,
    pub max_bitpool: u8,
}

impl SbcCaps {
    /// 标准高音质：44.1kHz / joint stereo / 16 子块 / 8 子带 / loudness。
pub fn standard() -> Self {
        SbcCaps {
            sampling_freq: SBC_SF_44K1,
            channel_mode: SBC_CH_JOINT,
            block_len: SBC_BLOCK_16,
            subbands: SBC_SUBBANDS_8,
            alloc: SBC_ALLOC_LOUDNESS,
            min_bitpool: 2,
            max_bitpool: 53,
        }
    }

    /// 低延迟配置：48kHz / mono / 12 子块 / 4 子带 / loudness。
pub fn low_latency() -> Self {
        SbcCaps {
            sampling_freq: SBC_SF_48K,
            channel_mode: SBC_CH_MONO,
            block_len: SBC_BLOCK_12,
            subbands: SBC_SUBBANDS_4,
            alloc: SBC_ALLOC_LOUDNESS,
            min_bitpool: 2,
            max_bitpool: 32,
        }
    }

    /// 能力位掩码（用于 AVDTP Get Capabilities 的“支持集合”表示）。
pub fn octet0(&self) -> u8 {
        (self.sampling_freq << 6)
            | (self.channel_mode << 4)
            | (self.block_len << 2)
            | (self.subbands << 1)
            | self.alloc
    }

    pub fn from_octet0(o: u8, min_bp: u8, max_bp: u8) -> Self {
        SbcCaps {
            sampling_freq: (o >> 6) & 3,
            channel_mode: (o >> 4) & 3,
            block_len: (o >> 2) & 3,
            subbands: (o >> 1) & 1,
            alloc: o & 1,
            min_bitpool: min_bp,
            max_bitpool: max_bp,
        }
    }
}

/// 构造 Media Codec 能力：`[0x07, 4, media_type, codec, octet0, min_bp, max_bp]`。
pub fn build_media_codec_sbc(caps: &SbcCaps, out: &mut [u8]) -> Option<usize> {
    if out.len() < 7 {
        return None;
    }
    out[0] = CAT_MEDIA_CODEC;
    out[1] = 4;
    out[2] = MEDIA_TYPE_AUDIO;
    out[3] = CODEC_SBC;
    out[4] = caps.octet0();
    out[5] = caps.min_bitpool;
    out[6] = caps.max_bitpool;
    Some(7)
}

/// 解析 Media Codec 能力（含类别头），返回 SBC 参数（仅 SBC）。
pub fn parse_media_codec(data: &[u8]) -> Option<SbcCaps> {
    if data.len() < 7 || data[0] != CAT_MEDIA_CODEC || data[2] != MEDIA_TYPE_AUDIO || data[3] != CODEC_SBC {
        return None;
    }
    Some(SbcCaps::from_octet0(data[4], data[5], data[6]))
}

// ==================== 信令消息构造 ====================

/// Discover：`[hdr(2)]`。
pub fn build_discover(tl: u8, out: &mut [u8]) -> Option<usize> {
    let mut f = [0u8; 2];
    let n = build_header(
        &AvdtpHeader { packet_type: PKT_SINGLE, transaction: tl, message_type: MSG_CMD, signal: SIG_DISCOVER },
        &mut f,
    )?;
    if out.len() < n {
        return None;
    }
    out[..n].copy_from_slice(&f[..n]);
    Some(n)
}

/// Discover 响应：每个端点一项 `[seid(1) | 0x00, type, media_type, role]`。
pub const SEID_TYPE_SOURCE: u8 = 0;
pub const SEID_TYPE_SINK: u8 = 1;
pub const SEID_ROLE_INITIATOR: u8 = 0;
pub const SEID_ROLE_ACCEPTOR: u8 = 1;

pub fn build_discover_response(tl: u8, seid: u8, ty: u8, accept: bool, out: &mut [u8]) -> Option<usize> {
    let mut f = [0u8; 2];
    let msg = if accept { MSG_RESPONSE_ACCEPT } else { MSG_RESPONSE_REJECT };
    if build_header(
        &AvdtpHeader { packet_type: PKT_SINGLE, transaction: tl, message_type: msg, signal: SIG_DISCOVER },
        &mut f,
    )
    .is_none()
    {
        return None;
    }
    if out.len() < 5 || !accept {
        return None;
    }
    out[0] = f[0];
    out[1] = f[1];
    out[2] = (seid & 0x3F) << 2;
    out[3] = 0; // media 类型（预留）
    // 端点类型/角色合并到一个字节：type<<1 | role（简化为本库表示）。
    out[4] = ty;
    Some(5)
}

/// Set Configuration：`[hdr, acp_seid, int_seid, caps...]`。
pub fn build_set_configuration(tl: u8, acp_seid: u8, int_seid: u8, caps: &[u8], out: &mut [u8]) -> Option<usize> {
    let head = AvdtpHeader { packet_type: PKT_SINGLE, transaction: tl, message_type: MSG_CMD, signal: SIG_SET_CONFIGURATION };
    let mut f = [0u8; 2];
    if build_header(&head, &mut f).is_none() {
        return None;
    }
    let need = 2 + 2 + caps.len();
    if out.len() < need {
        return None;
    }
    out[0] = f[0];
    out[1] = f[1];
    out[2] = (acp_seid & 0x3F) << 2;
    out[3] = (int_seid & 0x3F) << 2;
    out[4..need].copy_from_slice(caps);
    Some(need)
}

/// Open / Start / Close / Suspend / Abort（单字节载荷：seid）。
pub fn build_single_seid(tl: u8, msg: u8, signal: u8, seid: u8, out: &mut [u8]) -> Option<usize> {
    let head = AvdtpHeader { packet_type: PKT_SINGLE, transaction: tl, message_type: msg, signal };
    let mut f = [0u8; 2];
    if build_header(&head, &mut f).is_none() {
        return None;
    }
    if out.len() < 3 {
        return None;
    }
    out[0] = f[0];
    out[1] = f[1];
    out[2] = (seid & 0x3F) << 2;
    Some(3)
}

pub fn build_open(tl: u8, seid: u8, out: &mut [u8]) -> Option<usize> {
    build_single_seid(tl, MSG_CMD, SIG_OPEN, seid, out)
}
pub fn build_start(tl: u8, seid: u8, out: &mut [u8]) -> Option<usize> {
    build_single_seid(tl, MSG_CMD, SIG_START, seid, out)
}
pub fn build_close(tl: u8, seid: u8, out: &mut [u8]) -> Option<usize> {
    build_single_seid(tl, MSG_CMD, SIG_CLOSE, seid, out)
}
pub fn build_suspend(tl: u8, seid: u8, out: &mut [u8]) -> Option<usize> {
    build_single_seid(tl, MSG_CMD, SIG_SUSPEND, seid, out)
}
pub fn build_abort(tl: u8, seid: u8, out: &mut [u8]) -> Option<usize> {
    build_single_seid(tl, MSG_CMD, SIG_ABORT, seid, out)
}

// ==================== 媒体载荷（RTP + SBC 帧头） ====================

/// RTP 载荷类型（A2DP SBC 使用动态 96）。
pub const RTP_PT_SBC: u8 = 96;

/// AVDTP 媒体包：`RTP 头(12) + SBC 帧头(2) + 载荷`。
pub fn build_media_packet(
    seq: u16,
    timestamp: u32,
    ssrc: u32,
    caps: &SbcCaps,
    marker: bool,
    payload: &[u8],
    out: &mut [u8],
) -> Option<usize> {
    let need = 12 + 2 + payload.len();
    if out.len() < need {
        return None;
    }
    // RTP 头
    out[0] = 0x80 | (if marker { 0x80 } else { 0 }); // V=2, P=0, X=0, CC=0, M
    out[1] = RTP_PT_SBC;
    out[2..4].copy_from_slice(&seq.to_be_bytes());
    out[4..8].copy_from_slice(&timestamp.to_be_bytes());
    out[8..12].copy_from_slice(&ssrc.to_be_bytes());
    // SBC 帧头：0x9C + 编码参数
    out[12] = 0x9C;
    out[13] = (caps.sampling_freq << 6)
        | (caps.block_len << 4)
        | (caps.channel_mode << 2)
        | (caps.alloc << 1)
        | caps.subbands;
    out[14..need].copy_from_slice(payload);
    Some(need)
}

/// 解析媒体包：返回 `(rtp 头, sbc 头字节, 载荷)`。
pub fn parse_media_packet(frame: &[u8]) -> Option<([u8; 12], [u8; 2], &[u8])> {
    if frame.len() < 14 {
        return None;
    }
    let mut rtp = [0u8; 12];
    rtp.copy_from_slice(&frame[..12]);
    let mut sbc = [0u8; 2];
    sbc.copy_from_slice(&frame[12..14]);
    Some((rtp, sbc, &frame[14..]))
}

// ==================== 延迟估计 ====================

/// 按 SBC 参数估算单帧音频延迟（毫秒）：块长决定帧时长。
/// 16kHz→8ms/块… 实际帧时长 = 块数 × 采样数 / 采样率。
pub fn estimated_media_latency_ms(caps: &SbcCaps) -> u32 {
    let blocks: u32 = match caps.block_len {
        SBC_BLOCK_4 => 4,
        SBC_BLOCK_8 => 8,
        SBC_BLOCK_12 => 12,
        _ => 16,
    };
    let rate: u32 = match caps.sampling_freq {
        SBC_SF_16K => 16000,
        SBC_SF_32K => 32000,
        SBC_SF_44K1 => 44100,
        _ => 48000,
    };
    // 每个信道每块含 8（或 4）个子带样本×2；简化：帧时长 ≈ blocks*subsamples/rate。
    let subs: u32 = if caps.subbands == SBC_SUBBANDS_8 { 8 } else { 4 };
    let samples = blocks * subs;
    if rate == 0 {
        return 0;
    }
    (samples * 1000 + rate - 1) / rate
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let h = AvdtpHeader {
            packet_type: PKT_SINGLE,
            transaction: 3,
            message_type: MSG_CMD,
            signal: SIG_DISCOVER,
        };
        let mut b = [0u8; 2];
        build_header(&h, &mut b).unwrap();
        let (back, rest) = parse_header(&b).unwrap();
        assert_eq!(back, h);
        assert!(rest.is_empty());
        // octet0 = 0<<6 | 3<<2 | 0 = 0x0C
        assert_eq!(b[0], 0x0C);
        assert_eq!(b[1], SIG_DISCOVER);
    }

    #[test]
    fn sbc_caps_roundtrip() {
        let caps = SbcCaps::standard();
        assert_eq!(caps.octet0(), 0xBE); // 44.1k(2)<<6 | joint(3)<<4 | 16(3)<<2 | 8(1)<<1 | loud(0)
        let mut b = [0u8; 8];
        let n = build_media_codec_sbc(&caps, &mut b).unwrap();
        assert_eq!(b[0], CAT_MEDIA_CODEC);
        assert_eq!(b[2], MEDIA_TYPE_AUDIO);
        assert_eq!(b[3], CODEC_SBC);
        let back = parse_media_codec(&b[..n]).unwrap();
        assert_eq!(back.sampling_freq, SBC_SF_44K1);
        assert_eq!(back.channel_mode, SBC_CH_JOINT);
        assert_eq!(back.block_len, SBC_BLOCK_16);
        assert_eq!(back.max_bitpool, 53);
    }

    #[test]
    fn media_transport_and_delay_report() {
        let mut b = [0u8; 4];
        let n = build_media_transport(&mut b).unwrap();
        assert_eq!(&b[..n], &[CAT_MEDIA_TRANSPORT, 0]);
        let n = build_delay_reporting(&mut b).unwrap();
        assert_eq!(&b[..n], &[CAT_DELAY_REPORTING, 0]);
    }

    #[test]
    fn set_configuration_and_open() {
        let caps = SbcCaps::standard();
        let mut caps_bytes = [0u8; 4];
        caps_bytes[..2].copy_from_slice(&[CAT_MEDIA_TRANSPORT, 0]);
        let mut full = [0u8; 24];
        let mut caps_part = [0u8; 8];
        let n = build_media_codec_sbc(&caps, &mut caps_part).unwrap();
        let n2 = build_set_configuration(1, 3, 1, &caps_part[..n], &mut full).unwrap();
        assert_eq!(full[0] >> 2 & 0x0F, 1); // tl
        assert_eq!(full[1], SIG_SET_CONFIGURATION);
        assert_eq!(full[2] >> 2, 3); // acp seid
        assert_eq!(full[3] >> 2, 1); // int seid
        assert_eq!(full[4], CAT_MEDIA_CODEC);
        assert_eq!(full[6], MEDIA_TYPE_AUDIO);

        let mut b = [0u8; 4];
        let n = build_open(1, 3, &mut b).unwrap();
        assert_eq!(b[1], SIG_OPEN);
        assert_eq!(b[2] >> 2, 3);
    }

    #[test]
    fn media_packet_layout() {
        let caps = SbcCaps::standard();
        let mut out = [0u8; 32];
        let n = build_media_packet(1, 0x1234, 0x5678, &caps, true, b"audio", &mut out).unwrap();
        assert_eq!(n, 12 + 2 + 5);
        assert_eq!(out[0] & 0xC0, 0x80); // V=2
        assert_eq!(out[0] & 0x80, 0x80); // M
        assert_eq!(out[1], RTP_PT_SBC);
        assert_eq!(u16::from_be_bytes([out[2], out[3]]), 1);
        assert_eq!(u32::from_be_bytes([out[4], out[5], out[6], out[7]]), 0x1234);
        assert_eq!(out[12], 0x9C);
        assert_eq!(out[13] & 0xC0, SBC_SF_44K1 << 6);
        let (rtp, sbc, pl) = parse_media_packet(&out[..n]).unwrap();
        assert_eq!(&rtp[..2], &[0x80, 96]);
        assert_eq!(sbc[0], 0x9C);
        assert_eq!(pl, b"audio");
    }

    #[test]
    fn latency_estimate() {
        let l = SbcCaps::low_latency();
        let m = estimated_media_latency_ms(&l);
        // 12 块 × 4 子带 = 48 样本 / 48kHz = 1ms
        assert_eq!(m, 1);
        let s = SbcCaps::standard();
        assert!(estimated_media_latency_ms(&s) >= 2);
    }
}
