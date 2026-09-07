//! SMS PDU 编解码 Rust 包装：FFI 绑定 C 库（libpdu.a，见 build.rs）。
//!
//! 提供 rild 所需的安全封装：静态缓冲 + 明确的成功/失败返回。
//! 数据编码约定：dcs=0 → GSM 7-bit（单位 = septets）；dcs=8 → UCS-2（单位 = 字节）。

use core::ffi::c_int;

// ==================== FFI 声明 ====================

extern "C" {
    fn pdu_gsm7_encode(utf8: *const u8, utf8_len: c_int, septets: *mut u8, cap: c_int) -> c_int;
    fn pdu_gsm7_decode(septets: *const u8, n: c_int, utf8: *mut u8, cap: c_int) -> c_int;
    fn pdu_utf8_to_ucs2(utf8: *const u8, utf8_len: c_int, ucs2: *mut u8, cap: c_int) -> c_int;
    fn pdu_ucs2_to_utf8(ucs2: *const u8, len: c_int, utf8: *mut u8, cap: c_int) -> c_int;
    fn pdu_ud_units(text: *const u8, len: c_int, dcs: c_int) -> c_int;
    fn pdu_concat_parts(text: *const u8, len: c_int, dcs: c_int) -> c_int;
    fn pdu_concat_slice(
        text: *const u8,
        len: c_int,
        dcs: c_int,
        part: c_int,
        off: *mut c_int,
        len_out: *mut c_int,
    ) -> c_int;
    fn pdu_submit_encode(
        out: *mut u8,
        cap: c_int,
        sc: *const u8,
        sc_len: c_int,
        da: *const u8,
        da_len: c_int,
        text: *const u8,
        text_len: c_int,
        dcs: c_int,
        mr: u8,
        cref: c_int,
        ctotal: c_int,
        cpart: c_int,
        udl_out: *mut c_int,
    ) -> c_int;
    fn pdu_deliver_decode(
        pdu: *const u8,
        pdu_len: c_int,
        oa: *mut u8,
        oa_cap: c_int,
        oa_len: *mut c_int,
        scts: *mut u8,
        scts_cap: c_int,
        dcs: *mut c_int,
        text: *mut u8,
        text_cap: c_int,
        text_len: *mut c_int,
        cref: *mut c_int,
        ctotal: *mut c_int,
        cpart: *mut c_int,
    ) -> c_int;
}

// ==================== 常量 ====================

/// GSM 7-bit 数据编码（TP-DCS）。
pub const DCS_GSM7: u8 = 0;
/// UCS-2 数据编码（TP-DCS）。
pub const DCS_UCS2: u8 = 8;

/// PDU 缓冲上限（含 SMSC 字段）。
pub const PDU_CAP: usize = 200;
/// 正文 UTF-8 缓冲上限（长短信单段解码后的文本）。
pub const TEXT_CAP: usize = 200;

// ==================== 安全封装 ====================

/// 选择数据编码：文本可整体 GSM-7 编码 → 0；否则 → 8（UCS-2）。
pub fn choose_dcs(text: &[u8]) -> u8 {
    if unsafe { pdu_ud_units(text.as_ptr(), text.len() as c_int, 0) } >= 0 {
        DCS_GSM7
    } else {
        DCS_UCS2
    }
}

/// 长短信分段数（1 = 单条）。
pub fn concat_parts(text: &[u8], dcs: u8) -> u32 {
    let r = unsafe { pdu_concat_parts(text.as_ptr(), text.len() as c_int, dcs as c_int) };
    if r <= 1 {
        1
    } else {
        r as u32
    }
}

/// 构造 SMS-SUBMIT PDU（含 SMSC 字段）。返回 (PDU 字节切片, 用户数据长度)。
/// `mr` 消息引用；`(cref, ctotal, cpart)` 级联参数，ctotal<=1 时无 UDH。
/// PDU 写入内部静态缓冲（单线程 rild 独占），返回的切片有效至下次调用。
pub fn submit_encode(
    smsc: &[u8],
    da: &[u8],
    text: &[u8],
    dcs: u8,
    mr: u8,
    cref: u32,
    ctotal: u32,
    cpart: u32,
) -> Option<(&'static [u8], u32)> {
    static mut PDU_BUF: [u8; PDU_CAP] = [0; PDU_CAP];
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(PDU_BUF) };
    let mut udl: c_int = 0;
    let n = unsafe {
        pdu_submit_encode(
            buf.as_mut_ptr(),
            PDU_CAP as c_int,
            smsc.as_ptr(),
            smsc.len() as c_int,
            da.as_ptr(),
            da.len() as c_int,
            text.as_ptr(),
            text.len() as c_int,
            dcs as c_int,
            mr,
            cref as c_int,
            ctotal as c_int,
            cpart as c_int,
            &mut udl,
        )
    };
    if n < 0 {
        None
    } else {
        let slice = unsafe { core::slice::from_raw_parts(buf.as_ptr(), n as usize) };
        Some((slice, udl as u32))
    }
}

/// 第 `part` 段（1 起）的文本字节区间。
pub fn concat_slice(text: &[u8], dcs: u8, part: u32) -> Option<(usize, usize)> {
    let mut off: c_int = 0;
    let mut len: c_int = 0;
    let r = unsafe {
        pdu_concat_slice(
            text.as_ptr(),
            text.len() as c_int,
            dcs as c_int,
            part as c_int,
            &mut off,
            &mut len,
        )
    };
    if r == 1 {
        Some((off as usize, len as usize))
    } else {
        None
    }
}

/// 解析 SMS-DELIVER PDU（含 SMSC 字段）。
/// 返回 (发件人号码, SCTS "YY/MM/DD,HH:MM:SS+ZZ", DCS, 正文 UTF-8, 级联信息)。
pub struct Deliver {
    pub oa: [u8; 24],
    pub oa_len: usize,
    pub scts: [u8; 24],
    pub dcs: u8,
    pub text: [u8; TEXT_CAP],
    pub text_len: usize,
    pub concat_ref: u32,
    pub concat_total: u32,
    pub concat_part: u32,
}

pub fn deliver_decode(pdu: &[u8]) -> Option<Deliver> {
    let mut d = Deliver {
        oa: [0; 24],
        oa_len: 0,
        scts: [0; 24],
        dcs: 0,
        text: [0; TEXT_CAP],
        text_len: 0,
        concat_ref: 0,
        concat_total: 1,
        concat_part: 0,
    };
    let mut oa_len: c_int = 0;
    let mut dcs: c_int = 0;
    let mut text_len: c_int = 0;
    let mut cref: c_int = 0;
    let mut ctotal: c_int = 0;
    let mut cpart: c_int = 0;
    let r = unsafe {
        pdu_deliver_decode(
            pdu.as_ptr(),
            pdu.len() as c_int,
            d.oa.as_mut_ptr(),
            d.oa.len() as c_int,
            &mut oa_len,
            d.scts.as_mut_ptr(),
            d.scts.len() as c_int,
            &mut dcs,
            d.text.as_mut_ptr(),
            d.text.len() as c_int,
            &mut text_len,
            &mut cref,
            &mut ctotal,
            &mut cpart,
        )
    };
    if r < 0 {
        return None;
    }
    d.oa_len = oa_len as usize;
    d.dcs = dcs as u8;
    d.text_len = text_len as usize;
    d.concat_ref = cref as u32;
    d.concat_total = ctotal as u32;
    d.concat_part = cpart as u32;
    Some(d)
}
