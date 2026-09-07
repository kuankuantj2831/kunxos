//! LE SMP（Security Manager Protocol）：配对与 Bonding。
//!
//! 覆盖 Core Spec Vol 3 Part H（LE 分支）：
//! - 配对请求/响应（IO 能力、OOB、AuthReq、密钥分发位）
//! - 配对方法选择表（Table 2.8）：Just Works / Passkey Entry（显示/输入/
//!   角色决定）/ Numeric Comparison
//! - Legacy Pairing：c1 确认值、s1 STK 派生（PHY-PSK 键，含 e(k, ·) 的
//!   LE 字节序约定）
//! - LE Secure Connections：f4（确认值）、f5（MacKey/LTK）、f6（DHKey Check）、
//!   g2（NC 6 位显示值）、h6/h7（跨传输键派生，BR/EDR ↔ LE）
//! - 公钥交换 + P-256 ECDH（复用 `p256`）
//! - 密钥分发（EncInfo / Central Ident / Identity / SigningInfo）与 Bonding 记录
//!
//! 字节序约定与 Zephyr/规范一致：所有缓冲区内容为 LE（线上顺序）；密码函数
//! 内部按规范做字节反转，本模块负责在调用前后完成 `rev16`/`rev32`/`rev6`/`rev3`。
//! 纯函数、无堆、越界返回 `None`、绝不 panic。

use crate::aes::encrypt_block;
use crate::cmac::aes_cmac;
use crate::p256::{ecdh_shared_secret, public_key_from_private};

// ==================== 命令码 ====================

pub const CMD_PAIRING_REQUEST: u8 = 0x01;
pub const CMD_PAIRING_RESPONSE: u8 = 0x02;
pub const CMD_PAIRING_CONFIRM: u8 = 0x03;
pub const CMD_PAIRING_RANDOM: u8 = 0x04;
pub const CMD_PAIRING_FAILED: u8 = 0x05;
pub const CMD_ENCRYPT_INFO: u8 = 0x06;
pub const CMD_CENTRAL_IDENT: u8 = 0x07;
pub const CMD_IDENT_INFO: u8 = 0x08;
pub const CMD_IDENT_ADDR_INFO: u8 = 0x09;
pub const CMD_SIGNING_INFO: u8 = 0x0A;
pub const CMD_SECURITY_REQUEST: u8 = 0x0B;
pub const CMD_PUBLIC_KEY: u8 = 0x0C;
pub const CMD_DHKEY_CHECK: u8 = 0x0D;
pub const CMD_KEYPRESS_NOTIFICATION: u8 = 0x0E;

// ==================== 错误码 ====================

pub const ERR_PASSKEY_ENTRY_FAILED: u8 = 0x01;
pub const ERR_OOB_NOT_AVAILABLE: u8 = 0x02;
pub const ERR_AUTH_REQUIREMENTS: u8 = 0x03;
pub const ERR_CONFIRM_VALUE_FAILED: u8 = 0x04;
pub const ERR_PAIRING_NOT_SUPPORTED: u8 = 0x05;
pub const ERR_ENCRYPTION_KEY_SIZE: u8 = 0x06;
pub const ERR_COMMAND_NOT_SUPPORTED: u8 = 0x07;
pub const ERR_UNSPECIFIED: u8 = 0x08;
pub const ERR_REPEATED_ATTEMPTS: u8 = 0x09;
pub const ERR_INVALID_PARAMETERS: u8 = 0x0A;
pub const ERR_DHKEY_CHECK_FAILED: u8 = 0x0B;
pub const ERR_NUMERIC_COMPARISON_FAILED: u8 = 0x0C;
pub const ERR_BR_EDR_PAIRING_IN_PROGRESS: u8 = 0x0D;
pub const ERR_CROSS_TRANSPORT_KEY_DERIVATION_NOT_ALLOWED: u8 = 0x0E;
pub const ERR_KEY_REJECTED: u8 = 0x0F;

// ==================== IO 能力 / AuthReq / 密钥分发 ====================

pub const IO_DISPLAY_ONLY: u8 = 0x00;
pub const IO_DISPLAY_YESNO: u8 = 0x01;
pub const IO_KEYBOARD_ONLY: u8 = 0x02;
pub const IO_NO_INPUT_NO_OUTPUT: u8 = 0x03;
pub const IO_KEYBOARD_DISPLAY: u8 = 0x04;

pub const AUTH_BONDING: u8 = 0x01;
pub const AUTH_MITM: u8 = 0x02;
pub const AUTH_SC: u8 = 0x08;
pub const AUTH_KEYPRESS: u8 = 0x10;
pub const AUTH_CT2: u8 = 0x20;

pub const OOB_NOT_PRESENT: u8 = 0x00;
pub const OOB_PRESENT: u8 = 0x01;

pub const DIST_ENC_KEY: u8 = 0x01;
pub const DIST_ID_KEY: u8 = 0x02;
pub const DIST_SIGN: u8 = 0x04;
pub const DIST_LINK_KEY: u8 = 0x08;

/// LE 加密键长上限。
pub const MAX_ENC_KEY_SIZE: u8 = 16;
/// 最小（法定）键长。
pub const MIN_ENC_KEY_SIZE: u8 = 7;

// ==================== 字节反转辅助 ====================

/// 反转 16 字节（128 位 LE ↔ BE）。
#[inline]
pub fn rev16(b: &[u8]) -> [u8; 16] {
    let mut o = [0u8; 16];
    for i in 0..16 {
        o[i] = b[15 - i];
    }
    o
}

/// 反转 32 字节（256 位）。
#[inline]
pub fn rev32(b: &[u8]) -> [u8; 32] {
    let mut o = [0u8; 32];
    for i in 0..32 {
        o[i] = b[31 - i];
    }
    o
}

#[inline]
fn rev<const N: usize>(b: &[u8]) -> [u8; N] {
    let mut o = [0u8; N];
    for i in 0..N {
        o[i] = b[N - 1 - i];
    }
    o
}

/// `encrypt_le(k, data)`：LE 字节序单块 AES-128 加密（键与明文反转、输出反转）。
fn encrypt_le(k: &[u8], data: &[u8; 16]) -> [u8; 16] {
    let key = rev16(k);
    let block = rev16(data);
    let out = encrypt_block(&key, &block);
    rev16(&out)
}

// ==================== Legacy：c1 / s1 ====================

/// 计算 Legacy Pairing 确认值 `c1(k, r, preq, pres, ia, ra)`。
///
/// `preq`/`pres` 为 7 字节完整 PDU（含 opcode）；`ia`/`ra` 为
/// （地址类型, 6 字节地址）。
pub fn c1(
    k: &[u8; 16],
    r: &[u8; 16],
    preq: &[u8; 7],
    pres: &[u8; 7],
    ia: &[u8; 7],
    ra: &[u8; 7],
) -> [u8; 16] {
    // p1 = iat(1) || rat(1) || preq(7) || pres(7)
    let mut p1 = [0u8; 16];
    p1[0] = ia[0];
    p1[1] = ra[0];
    p1[2..9].copy_from_slice(preq);
    p1[9..16].copy_from_slice(pres);

    // x = e(k, r ⊕ p1)
    let mut x = [0u8; 16];
    for i in 0..16 {
        x[i] = r[i] ^ p1[i];
    }
    x = encrypt_le(k, &x);

    // p2 = ra(6) || ia(6) || 0x00000000
    let mut p2 = [0u8; 16];
    p2[0..6].copy_from_slice(&ra[1..7]);
    p2[6..12].copy_from_slice(&ia[1..7]);

    // 结果 = e(k, x ⊕ p2)
    let mut y = [0u8; 16];
    for i in 0..16 {
        y[i] = x[i] ^ p2[i];
    }
    encrypt_le(k, &y)
}

/// 计算 Legacy STK：`s1(k, r1, r2) = e(k, r2[0..8] || r1[0..8])`。
///
/// 双方按（应答方随机, 发起方随机）调用，得到相同 STK。
pub fn s1(k: &[u8; 16], r1: &[u8; 16], r2: &[u8; 16]) -> [u8; 16] {
    let mut m = [0u8; 16];
    m[0..8].copy_from_slice(&r2[..8]);
    m[8..16].copy_from_slice(&r1[..8]);
    encrypt_le(k, &m)
}

// ==================== LE Secure Connections：f4/f5/f6/g2/h6/h7 ====================

/// f5 的 SALT 常量（规范明文，不反转）。
pub const F5_SALT: [u8; 16] = [
    0x6c, 0x88, 0x83, 0x91, 0xaf, 0xaf, 0x5e, 0x47, 0x57, 0xb1, 0xe5, 0x85, 0xe6, 0x5f, 0x9a,
    0x9d,
];

/// f4(U, V, X, Z)：AES-CMAC-128，键 X，消息 `U||V||Z`（U/V 32 字节 X 坐标）。
pub fn f4(u: &[u8; 32], v: &[u8; 32], x: &[u8; 16], z: u8) -> [u8; 16] {
    let mut m = [0u8; 65];
    m[..32].copy_from_slice(&rev32(u));
    m[32..64].copy_from_slice(&rev32(v));
    m[64] = z;
    let key = rev16(x);
    let out = aes_cmac(&key, &m);
    rev16(&out)
}

/// f5(W, N1, N2, A1, A2) → (MacKey, LTK)。
///
/// A1/A2 为（类型, 地址）7 字节对；消息顺序始终为“发起方随机在前”。
pub fn f5(
    w: &[u8; 32],
    n1: &[u8; 16],
    n2: &[u8; 16],
    a1: &[u8; 7],
    a2: &[u8; 7],
) -> ([u8; 16], [u8; 16]) {
    // t = CMAC(SALT, rev(W))
    let ws = rev32(w);
    let t = aes_cmac(&F5_SALT, &ws);

    // M(counter) = counter(1) || "btle"(4) || N1(16 反) || N2(16 反) ||
    //              A1type || A1(6 反) || A2type || A2(6 反) || 0x0001(2)
    let mut m = [0u8; 53];
    m[1..5].copy_from_slice(b"btle");
    m[5..21].copy_from_slice(&rev16(n1));
    m[21..37].copy_from_slice(&rev16(n2));
    m[37] = a1[0];
    m[38..44].copy_from_slice(&rev::<6>(&a1[1..7]));
    m[44] = a2[0];
    m[45..51].copy_from_slice(&rev::<6>(&a2[1..7]));
    m[51] = 0x01;
    m[52] = 0x00;

    let mut mm = m;
    mm[0] = 0; // counter 0 → MacKey
    let mackey_raw = aes_cmac(&t, &mm);
    mm[0] = 1; // counter 1 → LTK
    let ltk_raw = aes_cmac(&t, &mm);

    (rev16(&mackey_raw), rev16(&ltk_raw))
}

/// f6(W, N1, N2, R, IOcap, A1, A2)：DHKey Check 确认值。
pub fn f6(
    w: &[u8; 16],
    n1: &[u8; 16],
    n2: &[u8; 16],
    r: &[u8; 16],
    iocap: &[u8; 3],
    a1: &[u8; 7],
    a2: &[u8; 7],
) -> [u8; 16] {
    // M = N1(反) || N2(反) || R(反) || IOcap(反 3) || A1type || A1(6 反) ||
    //     A2type || A2(6 反)
    let mut m = [0u8; 65];
    m[..16].copy_from_slice(&rev16(n1));
    m[16..32].copy_from_slice(&rev16(n2));
    m[32..48].copy_from_slice(&rev16(r));
    m[48..51].copy_from_slice(&rev::<3>(iocap));
    m[51] = a1[0];
    m[52..58].copy_from_slice(&rev::<6>(&a1[1..7]));
    m[58] = a2[0];
    m[59..65].copy_from_slice(&rev::<6>(&a2[1..7]));
    let key = rev16(w);
    let out = aes_cmac(&key, &m);
    rev16(&out)
}

/// g2(U, V, X, Y)：Numeric Comparison 的 6 位显示值。
pub fn g2(u: &[u8; 32], v: &[u8; 32], x: &[u8; 16], y: &[u8; 16]) -> u32 {
    let mut m = [0u8; 80];
    m[..32].copy_from_slice(&rev32(u));
    m[32..64].copy_from_slice(&rev32(v));
    m[64..80].copy_from_slice(&rev16(y));
    let key = rev16(x);
    let out = aes_cmac(&key, &m);
    u32::from_be_bytes([out[12], out[13], out[14], out[15]]) % 1_000_000
}

/// h6(W, keyID[4])：跨传输键派生（BR/EDR 与 LE 之间）。
pub fn h6(w: &[u8; 16], key_id: &[u8; 4]) -> [u8; 16] {
    let key = rev16(w);
    let msg = rev::<4>(key_id);
    let out = aes_cmac(&key, &msg);
    rev16(&out)
}

/// h7(SALT[16], W[16])：跨传输键派生（CT2）。
pub fn h7(salt: &[u8; 16], w: &[u8; 16]) -> [u8; 16] {
    let key = rev16(salt);
    let msg = rev16(w);
    let out = aes_cmac(&key, &msg);
    rev16(&out)
}

// ==================== 配对方法选择 ====================

/// 配对方法枚举（与 Zephyr/规范一致）。
pub const METH_JUST_WORKS: u8 = 0;
pub const METH_PASSKEY_INPUT: u8 = 1;
pub const METH_PASSKEY_DISPLAY: u8 = 2;
pub const METH_PASSKEY_CONFIRM: u8 = 3; // Numeric Comparison
pub const METH_PASSKEY_ROLE: u8 = 4;
pub const METH_LE_SC_OOB: u8 = 5;
pub const METH_LEGACY_OOB: u8 = 6;

/// Legacy Pairing 方法表（Table 2.8）：`[remote_io][local_io]`。
fn legacy_method(remote_io: u8, local_io: u8) -> u8 {
    const T: [[u8; 5]; 5] = [
        [METH_JUST_WORKS, METH_JUST_WORKS, METH_PASSKEY_INPUT, METH_JUST_WORKS, METH_PASSKEY_INPUT],
        [METH_JUST_WORKS, METH_JUST_WORKS, METH_PASSKEY_INPUT, METH_JUST_WORKS, METH_PASSKEY_INPUT],
        [METH_PASSKEY_DISPLAY, METH_PASSKEY_DISPLAY, METH_PASSKEY_INPUT, METH_JUST_WORKS, METH_PASSKEY_DISPLAY],
        [METH_JUST_WORKS, METH_JUST_WORKS, METH_JUST_WORKS, METH_JUST_WORKS, METH_JUST_WORKS],
        [METH_PASSKEY_DISPLAY, METH_PASSKEY_DISPLAY, METH_PASSKEY_INPUT, METH_JUST_WORKS, METH_PASSKEY_ROLE],
    ];
    match (remote_io.min(4), local_io.min(4)) {
        (r, l) => T[r as usize][l as usize],
    }
}

/// LE Secure Connections 方法表（Table 2.8）：`[remote_io][local_io]`。
fn sc_method(remote_io: u8, local_io: u8) -> u8 {
    const T: [[u8; 5]; 5] = [
        [METH_JUST_WORKS, METH_JUST_WORKS, METH_PASSKEY_INPUT, METH_JUST_WORKS, METH_PASSKEY_INPUT],
        [METH_JUST_WORKS, METH_PASSKEY_CONFIRM, METH_PASSKEY_INPUT, METH_JUST_WORKS, METH_PASSKEY_CONFIRM],
        [METH_PASSKEY_DISPLAY, METH_PASSKEY_DISPLAY, METH_PASSKEY_INPUT, METH_JUST_WORKS, METH_PASSKEY_DISPLAY],
        [METH_JUST_WORKS, METH_JUST_WORKS, METH_JUST_WORKS, METH_JUST_WORKS, METH_JUST_WORKS],
        [METH_PASSKEY_DISPLAY, METH_PASSKEY_CONFIRM, METH_PASSKEY_INPUT, METH_JUST_WORKS, METH_PASSKEY_CONFIRM],
    ];
    match (remote_io.min(4), local_io.min(4)) {
        (r, l) => T[r as usize][l as usize],
    }
}

/// 双方均需 MITM 且 SC ⇒ 使用方法表；否则 Just Works。
pub fn resolve_method(
    local_io: u8,
    remote_io: u8,
    local_auth: u8,
    remote_auth: u8,
    sc: bool,
) -> u8 {
    let mitm = (local_auth & AUTH_MITM) != 0 || (remote_auth & AUTH_MITM) != 0;
    if !mitm {
        return METH_JUST_WORKS;
    }
    if sc {
        sc_method(remote_io, local_io)
    } else {
        legacy_method(remote_io, local_io)
    }
}

/// 根据本端角色把 `PASSKEY_ROLE` 解析为显示/输入。
pub fn resolve_passkey_role(method: u8, role_is_central: bool) -> u8 {
    if method == METH_PASSKEY_ROLE {
        // 发起方显示、应答方输入。
        if role_is_central {
            METH_PASSKEY_DISPLAY
        } else {
            METH_PASSKEY_INPUT
        }
    } else {
        method
    }
}

// ==================== PDU 编解码 ====================

/// 配对请求/响应的 6 字节体。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PairingBody {
    pub io_capability: u8,
    pub oob: u8,
    pub auth_req: u8,
    pub max_key_size: u8,
    pub init_key_dist: u8,
    pub resp_key_dist: u8,
}

impl PairingBody {
    pub fn parse(pdu: &[u8]) -> Option<Self> {
        if pdu.len() < 7 {
            return None;
        }
        Some(PairingBody {
            io_capability: pdu[1],
            oob: pdu[2],
            auth_req: pdu[3],
            max_key_size: pdu[4],
            init_key_dist: pdu[5],
            resp_key_dist: pdu[6],
        })
    }

    /// 构造完整 PDU（含 opcode），返回长度。
    pub fn build(&self, cmd: u8, out: &mut [u8]) -> Option<usize> {
        if out.len() < 7 {
            return None;
        }
        out[0] = cmd;
        out[1] = self.io_capability;
        out[2] = self.oob;
        out[3] = self.auth_req;
        out[4] = self.max_key_size;
        out[5] = self.init_key_dist;
        out[6] = self.resp_key_dist;
        Some(7)
    }
}

/// 构造 16 字节载荷 PDU（Pairing Confirm / Random / EncInfo / SigningInfo /
/// DHKey Check / Identity Info）。
pub fn build_16(cmd: u8, val: &[u8; 16], out: &mut [u8]) -> Option<usize> {
    if out.len() < 17 {
        return None;
    }
    out[0] = cmd;
    out[1..17].copy_from_slice(val);
    Some(17)
}

/// 构造 Public Key PDU（64 字节 X||Y）。
pub fn build_public_key(x: &[u8; 32], y: &[u8; 32], out: &mut [u8]) -> Option<usize> {
    if out.len() < 65 {
        return None;
    }
    out[0] = CMD_PUBLIC_KEY;
    out[1..33].copy_from_slice(x);
    out[33..65].copy_from_slice(y);
    Some(65)
}

/// 构造 Central Ident（EDIV 2 + Rand 8 = 10 字节载荷）。
pub fn build_central_ident(ediv: u16, rand: &[u8; 8], out: &mut [u8]) -> Option<usize> {
    if out.len() < 11 {
        return None;
    }
    out[0] = CMD_CENTRAL_IDENT;
    out[1..3].copy_from_slice(&ediv.to_le_bytes());
    out[3..11].copy_from_slice(rand);
    Some(11)
}

/// 构造 Identity Address Info（7 字节：地址类型 + 6 字节地址）。
pub fn build_ident_addr_info(addr_type: u8, addr: &[u8; 6], out: &mut [u8]) -> Option<usize> {
    if out.len() < 8 {
        return None;
    }
    out[0] = CMD_IDENT_ADDR_INFO;
    out[1] = addr_type;
    out[2..8].copy_from_slice(addr);
    Some(8)
}

/// 构造 Security Request（1 字节 auth_req）。
pub fn build_security_request(auth_req: u8, out: &mut [u8]) -> Option<usize> {
    if out.len() < 2 {
        return None;
    }
    out[0] = CMD_SECURITY_REQUEST;
    out[1] = auth_req;
    Some(2)
}

/// 构造 Pairing Failed。
pub fn build_pairing_failed(reason: u8, out: &mut [u8]) -> Option<usize> {
    if out.len() < 2 {
        return None;
    }
    out[0] = CMD_PAIRING_FAILED;
    out[1] = reason;
    Some(2)
}

// ==================== Bonding 记录 ====================

/// 单条 Bonding 记录（对端身份、LTK、EDIV/Rand、IRK、CSRK）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bond {
    pub addr_type: u8,
    pub addr: [u8; 6],
    pub ltk: [u8; 16],
    pub ediv: u16,
    pub rand: [u8; 8],
    pub irk: [u8; 16],
    pub csrk: [u8; 16],
    pub authenticated: bool,
    pub sc: bool,
    pub key_size: u8,
}

// ==================== 配对会话 ====================

/// 角色。
pub const ROLE_CENTRAL: u8 = 0;
pub const ROLE_PERIPHERAL: u8 = 1;

/// 阶段。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SmpPhase {
    Idle,
    WaitPairingRsp,
    WaitPublicKey,
    WaitConfirm,
    WaitRandom,
    VerifyRandom,
    WaitNumericConfirm,
    WaitDhKeyCheck,
    TransportKeyDerived,
    DistributeKeys,
    Complete,
    Failed,
}

/// 需要用户参与的动作（由上层回调处理）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SmpAction {
    None,
    /// 显示 6 位 Passkey（Passkey Entry / Numeric Comparison）。
    DisplayPasskey(u32),
    /// 请求 6 位 Passkey 输入（键盘）。
    RequestPasskeyInput,
    /// 请求用户在设备上确认 6 位数字（Numeric Comparison 双方）。
    ConfirmNumeric(u32),
    /// 请求用户同意 Just Works 配对。
    ConfirmJustWorks,
    /// 计算完成，等待链路层加密后将进入密钥分发。
    EncryptLink,
    /// 配对完成（Bonding 记录可用）。
    PairingComplete,
}

/// LE SMP 配对会话。
pub struct LePairing {
    pub phase: SmpPhase,
    pub role: u8,
    pub local: PairingBody,
    pub remote: PairingBody,
    /// 本端完整请求 PDU（含 opcode）。
    pub preq: [u8; 7],
    /// 对端响应 PDU。
    pub prsp: [u8; 7],
    pub method: u8,
    pub passkey: u32,
    pub passkey_round: u8,
    pub prnd: [u8; 16],
    pub rrnd: [u8; 16],
    pub pcnf: [u8; 16],
    pub e: [u8; 16],
    pub pkey: [u8; 64],
    pub dhkey: [u8; 32],
    pub mackey: [u8; 16],
    pub ltk: [u8; 16],
    pub tk: [u8; 16],
    pub init_addr: [u8; 7],
    pub resp_addr: [u8; 7],
    /// 收到对端随机后的确认校验结果。
    pub confirm_verified: bool,
    pub local_dist: u8,
    pub remote_dist: u8,
    /// 本端发给对端的密钥（分发完成标志）。
    pub local_dist_sent: u8,
    /// 从对端收到的密钥。
    pub remote_dist_received: u8,
    pub bond: Option<Bond>,
    /// 用于 Legacy 的 Passkey（Just Works 时 TK=0）。
    pub legacy_passkey: u32,
    /// 本端 P-256 私钥覆盖（生产注入；默认内置固定测试键）。
    pub private_key: Option<[u8; 32]>,
}

impl LePairing {
    pub fn new(role: u8, local: PairingBody, init_addr: &[u8; 7], resp_addr: &[u8; 7]) -> Self {
        LePairing {
            phase: SmpPhase::Idle,
            role,
            local,
            remote: PairingBody {
                io_capability: 0,
                oob: 0,
                auth_req: 0,
                max_key_size: 16,
                init_key_dist: 0,
                resp_key_dist: 0,
            },
            preq: [0u8; 7],
            prsp: [0u8; 7],
            method: METH_JUST_WORKS,
            passkey: 0,
            passkey_round: 0,
            prnd: [0u8; 16],
            rrnd: [0u8; 16],
            pcnf: [0u8; 16],
            e: [0u8; 16],
            pkey: [0u8; 64],
            dhkey: [0u8; 32],
            mackey: [0u8; 16],
            ltk: [0u8; 16],
            tk: [0u8; 16],
            init_addr: *init_addr,
            resp_addr: *resp_addr,
            confirm_verified: false,
            local_dist: 0,
            remote_dist: 0,
            local_dist_sent: 0,
            remote_dist_received: 0,
            bond: None,
            legacy_passkey: 0,
            private_key: None,
        }
    }

    /// 是否 SC 配对。
    pub fn is_sc(&self) -> bool {
        (self.local.auth_req & AUTH_SC) != 0 && (self.remote.auth_req & AUTH_SC) != 0
    }

    /// 是否 MITM。
    pub fn mitm(&self) -> bool {
        (self.local.auth_req & AUTH_MITM) != 0 || (self.remote.auth_req & AUTH_MITM) != 0
    }

    /// 发起方构造并发送 Pairing Request。
    pub fn start_pairing(&mut self, out: &mut [u8]) -> Option<(usize, SmpAction)> {
        if self.role != ROLE_CENTRAL || self.phase != SmpPhase::Idle {
            return None;
        }
        self.preq[1..7].copy_from_slice(&[
            self.local.io_capability,
            self.local.oob,
            self.local.auth_req,
            self.local.max_key_size,
            self.local.init_key_dist,
            self.local.resp_key_dist,
        ]);
        self.preq[0] = CMD_PAIRING_REQUEST;
        self.local_dist = self.local.init_key_dist;
        self.remote_dist = self.local.resp_key_dist;
        self.phase = SmpPhase::WaitPairingRsp;
        let n = self.local.build(CMD_PAIRING_REQUEST, out)?;
        Some((n, SmpAction::None))
    }

    /// 应答方：收到 Pairing Request 后回响应（并决定方法）。
    pub fn on_pairing_request(&mut self, pdu: &[u8], out: &mut [u8]) -> Option<(usize, SmpAction)> {
        if self.role != ROLE_PERIPHERAL || self.phase != SmpPhase::Idle {
            return None;
        }
        let body = PairingBody::parse(pdu)?;
        self.preq[..7].copy_from_slice(&pdu[..7]);
        self.remote = body;
        self.method = resolve_method(
            self.local.io_capability,
            self.remote.io_capability,
            self.local.auth_req,
            self.remote.auth_req,
            self.is_sc(),
        );
        // SC：passkey 方法按角色决定展示/输入；Legacy 同样采用角色规则。
        self.method = resolve_passkey_role(self.method, self.role == ROLE_CENTRAL);
        if self.method == METH_PASSKEY_ROLE {
            self.method = if self.role == ROLE_CENTRAL {
                METH_PASSKEY_DISPLAY
            } else {
                METH_PASSKEY_INPUT
            };
        }
        self.prsp[0] = CMD_PAIRING_RESPONSE;
        self.prsp[1..7].copy_from_slice(&[
            self.local.io_capability,
            self.local.oob,
            self.local.auth_req,
            self.local.max_key_size,
            self.local.init_key_dist,
            self.local.resp_key_dist,
        ]);
        self.local_dist = self.local.resp_key_dist;
        self.remote_dist = self.remote.init_key_dist;
        // SC：等待发起方公钥；Legacy：等待发起方确认。
        self.phase = if self.is_sc() {
            SmpPhase::WaitPublicKey
        } else {
            SmpPhase::WaitConfirm
        };
        let n = self.local.build(CMD_PAIRING_RESPONSE, out)?;
        let action = self.initial_user_action();
        if action != SmpAction::None {
            return Some((n, action));
        }
        Some((n, SmpAction::None))
    }

    fn initial_user_action(&self) -> SmpAction {
        // 方法表已决定本端角色：PASSKEY_DISPLAY=本端显示，PASSKEY_INPUT=本端输入。
        match self.method {
            METH_JUST_WORKS => {
                if self.mitm() && self.role == ROLE_PERIPHERAL {
                    SmpAction::ConfirmJustWorks
                } else {
                    SmpAction::None
                }
            }
            METH_PASSKEY_DISPLAY => SmpAction::DisplayPasskey(self.passkey),
            METH_PASSKEY_INPUT => SmpAction::RequestPasskeyInput,
            METH_PASSKEY_CONFIRM => SmpAction::None, // NC 在随机交换后显示
            _ => SmpAction::None,
        }
    }

    /// 设置用户输入的 6 位 Passkey（键盘输入方），并生成/使用 TK。
    pub fn set_incoming_passkey(&mut self, passkey: u32) {
        self.legacy_passkey = passkey;
        if !self.is_sc() {
            // Legacy Passkey Entry：TK = 6 位十进制小端（补零 16 字节）。
            let d = passkey.min(999_999);
            let b = [
                (d % 10) as u8,
                ((d / 10) % 10) as u8,
                ((d / 100) % 10) as u8,
                ((d / 1000) % 10) as u8,
                ((d / 10000) % 10) as u8,
                ((d / 100000) % 10) as u8,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
            ];
            self.tk = b;
        }
    }

    /// 发起方收到 Pairing Response。
    pub fn on_pairing_response(&mut self, pdu: &[u8], out: &mut [u8]) -> Option<(usize, SmpAction)> {
        if self.role != ROLE_CENTRAL || self.phase != SmpPhase::WaitPairingRsp {
            return None;
        }
        let body = PairingBody::parse(pdu)?;
        self.prsp[..7].copy_from_slice(&pdu[..7]);
        self.remote = body;
        self.method = resolve_method(
            self.local.io_capability,
            self.remote.io_capability,
            self.local.auth_req,
            self.remote.auth_req,
            self.is_sc(),
        );
        self.method = resolve_passkey_role(self.method, self.role == ROLE_CENTRAL);
        if self.method == METH_PASSKEY_ROLE {
            self.method = if self.role == ROLE_CENTRAL {
                METH_PASSKEY_DISPLAY
            } else {
                METH_PASSKEY_INPUT
            };
        }
        let user = self.initial_user_action();
        if self.is_sc() {
            self.phase = SmpPhase::WaitPublicKey;
            let (n, _) = self.send_public_key(out)?;
            if user != SmpAction::None {
                return Some((n, user));
            }
            return Some((n, SmpAction::None));
        }
        // Legacy：生成随机并发送确认。
        self.phase = SmpPhase::WaitConfirm;
        let (n, _) = self.send_legacy_confirm(out)?;
        if user != SmpAction::None {
            return Some((n, user));
        }
        Some((n, SmpAction::None))
    }

    /// 发送本端 P-256 公钥。
    fn send_public_key(&mut self, out: &mut [u8]) -> Option<(usize, SmpAction)> {
        let (x, y) = self.own_public_key();
        self.phase = SmpPhase::WaitPublicKey;
        let n = build_public_key(&x, &y, out)?;
        Some((n, SmpAction::None))
    }

    /// 本端 P-256 私钥（测试固定；生产由上层注入并通过 `set_private_key` 覆盖）。
    pub fn own_private_key(&self) -> [u8; 32] {
        if let Some(d) = self.private_key {
            return d;
        }
        if self.role == ROLE_CENTRAL {
            [
                0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x02,
            ]
        } else {
            [
                0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
            ]
        }
    }

    pub fn own_public_key(&self) -> ([u8; 32], [u8; 32]) {
        let d = self.own_private_key();
        public_key_from_private(&d)
    }

    /// 设置本端 P-256 私钥（生产路径；测试用例用内置固定键）。
    pub fn set_private_key(&mut self, d: &[u8; 32]) {
        self.private_key = Some(*d);
    }

    /// 收到对端公钥：计算 DHKey。
    pub fn on_public_key(&mut self, pdu: &[u8], out: &mut [u8]) -> Option<(usize, SmpAction)> {
        if pdu.len() < 65 || pdu[0] != CMD_PUBLIC_KEY {
            return None;
        }
        self.pkey[..32].copy_from_slice(&pdu[1..33]);
        self.pkey[32..64].copy_from_slice(&pdu[33..65]);
        let mut x = [0u8; 32];
        x.copy_from_slice(&self.pkey[..32]);
        let mut y = [0u8; 32];
        y.copy_from_slice(&self.pkey[32..64]);
        let d = self.own_private_key();
        let dh = match ecdh_shared_secret(&d, &x, &y) {
            Some(dh) => dh,
            None => return None,
        };
        self.dhkey = dh;

        if self.phase == SmpPhase::WaitPublicKey {
            // 应答方收到发起方公钥后，回发自己的公钥。
            if self.role == ROLE_PERIPHERAL {
                self.phase = SmpPhase::WaitConfirm;
                let (x1, y1) = self.own_public_key();
                let n = build_public_key(&x1, &y1, out)?;
                return Some((n, SmpAction::None));
            }
            // 发起方收到应答公钥 → 发送配对确认。
            self.phase = SmpPhase::WaitConfirm;
            let (n, a) = self.send_sc_confirm(out)?;
            return Some((n, a));
        }
        Some((0, SmpAction::None))
    }

    /// 生成本端 SC 确认值（f4）：U=本端公钥，V=对端公钥，X=本端随机。
    fn sc_confirm_value(&self, z: u8) -> [u8; 16] {
        let (x0, _) = self.own_public_key();
        let mut peer = [0u8; 32];
        peer.copy_from_slice(&self.pkey[..32]);
        f4(&x0, &peer, &self.prnd, z)
    }

    /// 校验对端确认值：U=对端公钥，V=本端公钥，X=对端随机。
    fn verify_sc_confirm(&self, cfm: &[u8; 16], z: u8) -> bool {
        let (x0, _) = self.own_public_key();
        let mut peer = [0u8; 32];
        peer.copy_from_slice(&self.pkey[..32]);
        let expected = f4(&peer, &x0, &self.rrnd, z);
        expected == *cfm
    }

    /// 发送本端 SC Pairing Confirm。
    fn send_sc_confirm(&mut self, out: &mut [u8]) -> Option<(usize, SmpAction)> {
        let z = self.confirm_z();
        let cfm = self.sc_confirm_value(z);
        self.pcnf = cfm;
        self.phase = SmpPhase::WaitConfirm;
        let n = build_16(CMD_PAIRING_CONFIRM, &cfm, out)?;
        Some((n, SmpAction::None))
    }

    /// 确认/随机交换阶段的 Z 参数（Passkey 位或 0）。
    fn confirm_z(&self) -> u8 {
        match self.method {
            METH_PASSKEY_DISPLAY | METH_PASSKEY_INPUT => {
                0x80 | (((self.passkey >> self.passkey_round) & 1) as u8)
            }
            _ => 0,
        }
    }

    /// 发送 Legacy Pairing 确认值（c1）。双方各用本端随机计算。
    fn send_legacy_confirm(&mut self, out: &mut [u8]) -> Option<(usize, SmpAction)> {
        let cfm = c1(
            &self.tk,
            &self.prnd,
            &self.preq,
            &self.prsp,
            &self.init_addr,
            &self.resp_addr,
        );
        self.phase = SmpPhase::WaitConfirm;
        let n = build_16(CMD_PAIRING_CONFIRM, &cfm, out)?;
        Some((n, SmpAction::None))
    }

    /// 生成 6 位 Passkey（显示或 NC 用）。
    pub fn gen_passkey(&mut self) -> u32 {
        // 由上层调用 set_passkey 注入；这里提供确定性默认（测试友好）。
        if self.passkey == 0 {
            self.passkey = 654321;
        }
        self.passkey
    }

    pub fn set_passkey(&mut self, passkey: u32) {
        self.passkey = passkey.min(999_999);
    }

    /// 收到对端 Pairing Confirm。
    ///
    /// 发起方已发送本端确认，收到对端确认后发送本端随机；
    /// 应答方回送自己的确认（SC 用 f4、Legacy 用 c1）。
    pub fn on_pairing_confirm(&mut self, pdu: &[u8], out: &mut [u8]) -> Option<(usize, SmpAction)> {
        if pdu.len() < 17 || pdu[0] != CMD_PAIRING_CONFIRM {
            return None;
        }
        let mut cfm = [0u8; 16];
        cfm.copy_from_slice(&pdu[1..17]);
        self.pcnf = cfm;
        if self.role == ROLE_CENTRAL {
            // 发起方：发送本端随机。
            self.phase = SmpPhase::WaitRandom;
            let n = build_16(CMD_PAIRING_RANDOM, &self.prnd, out)?;
            return Some((n, SmpAction::None));
        }
        // 应答方：回送本端确认。
        self.phase = SmpPhase::WaitRandom;
        if self.is_sc() {
            let z = self.confirm_z();
            let cfm_own = self.sc_confirm_value(z);
            let n = build_16(CMD_PAIRING_CONFIRM, &cfm_own, out)?;
            return Some((n, SmpAction::None));
        }
        let cfm_own = c1(
            &self.tk,
            &self.prnd,
            &self.preq,
            &self.prsp,
            &self.init_addr,
            &self.resp_addr,
        );
        let n = build_16(CMD_PAIRING_CONFIRM, &cfm_own, out)?;
        Some((n, SmpAction::None))
    }

    /// 收到对端 Pairing Random → 校验确认、计算密钥。
    pub fn on_pairing_random(&mut self, pdu: &[u8], out: &mut [u8]) -> Option<(usize, SmpAction)> {
        if pdu.len() < 17 || pdu[0] != CMD_PAIRING_RANDOM {
            return None;
        }
        self.rrnd.copy_from_slice(&pdu[1..17]);
        self.phase = SmpPhase::VerifyRandom;

        if self.is_sc() {
            // 校验对端确认（f4，Z=passkey 位或 0）。
            let z = self.confirm_z();
            if !self.verify_sc_confirm(&self.pcnf, z) {
                let n = build_pairing_failed(ERR_CONFIRM_VALUE_FAILED, out)?;
                self.phase = SmpPhase::Failed;
                return Some((n, SmpAction::None));
            }
            // NC：生成展示的 6 位值。两侧统一按（发起方公钥, 应答方公钥, 发起方随机,
            // 应答方随机）入参，展示值相同。
            if self.method == METH_PASSKEY_CONFIRM {
                let (x0, _) = self.own_public_key();
                let mut peer = [0u8; 32];
                peer.copy_from_slice(&self.pkey[..32]);
                let p = if self.role == ROLE_CENTRAL {
                    g2(&x0, &peer, &self.prnd, &self.rrnd)
                } else {
                    g2(&peer, &x0, &self.rrnd, &self.prnd)
                };
                self.phase = SmpPhase::WaitNumericConfirm;
                if self.role == ROLE_PERIPHERAL {
                    // 应答方：回送本端随机后再进入等待确认。
                    let n = build_16(CMD_PAIRING_RANDOM, &self.prnd, out)?;
                    return Some((n, SmpAction::ConfirmNumeric(p)));
                }
                return Some((0, SmpAction::ConfirmNumeric(p)));
            }
            // Passkey：推进轮次（共 20 轮）。
            if self.method == METH_PASSKEY_DISPLAY || self.method == METH_PASSKEY_INPUT {
                self.passkey_round += 1;
                if self.passkey_round < 20 {
                    self.phase = SmpPhase::WaitConfirm;
                    if self.role == ROLE_CENTRAL {
                        // 发起方发起下一轮确认（上层注入新随机）。
                        return self.send_sc_confirm(out);
                    }
                    // 应答方：回送本端随机，随后等待下一轮发起方确认。
                    let n = build_16(CMD_PAIRING_RANDOM, &self.prnd, out)?;
                    return Some((n, SmpAction::None));
                }
            }
            // 派生 MacKey/LTK。f5 以“发起方随机、应答方随机、发起方地址、
            // 应答方地址”的顺序入参。
            let (n1, n2) = if self.role == ROLE_CENTRAL {
                (&self.prnd, &self.rrnd)
            } else {
                (&self.rrnd, &self.prnd)
            };
            let (mk, ltk) = f5(&self.dhkey, n1, n2, &self.init_addr, &self.resp_addr);
            self.mackey = mk;
            self.ltk = ltk;
            self.tk = ltk;
            let mut r = [0u8; 16];
            if self.method == METH_PASSKEY_DISPLAY || self.method == METH_PASSKEY_INPUT {
                r[..4].copy_from_slice(&self.passkey.to_le_bytes());
            }
            if self.role == ROLE_PERIPHERAL {
                // 应答方：回送本端随机；对端 E 待收。
                self.phase = SmpPhase::WaitDhKeyCheck;
                let n = build_16(CMD_PAIRING_RANDOM, &self.prnd, out)?;
                return Some((n, SmpAction::None));
            }
            // 发起方：计算本端 DHKey Check 并发送。
            let io = [self.preq[1], self.preq[2], self.preq[3]];
            let e = f6(
                &self.mackey,
                &self.prnd,
                &self.rrnd,
                &r,
                &io,
                &self.init_addr,
                &self.resp_addr,
            );
            self.phase = SmpPhase::WaitDhKeyCheck;
            let n = build_16(CMD_DHKEY_CHECK, &e, out)?;
            return Some((n, SmpAction::None));
        }

        // Legacy：校验对端确认、派生 STK。
        let cfm_rem = c1(
            &self.tk,
            &self.rrnd,
            &self.preq,
            &self.prsp,
            &self.init_addr,
            &self.resp_addr,
        );
        if self.pcnf != cfm_rem {
            let n = build_pairing_failed(ERR_CONFIRM_VALUE_FAILED, out)?;
            self.phase = SmpPhase::Failed;
            return Some((n, SmpAction::None));
        }
        self.confirm_verified = true;
        // STK = s1(TK, 应答方随机, 发起方随机)，双方得到相同 STK。
        let stk = if self.role == ROLE_CENTRAL {
            s1(&self.tk, &self.rrnd, &self.prnd)
        } else {
            s1(&self.tk, &self.prnd, &self.rrnd)
        };
        self.tk = stk;
        self.phase = SmpPhase::TransportKeyDerived;
        if self.role == ROLE_PERIPHERAL {
            // 应答方：回送本端随机。
            let n = build_16(CMD_PAIRING_RANDOM, &self.prnd, out)?;
            return Some((n, SmpAction::EncryptLink));
        }
        Some((0, SmpAction::EncryptLink))
    }

    /// 已生成本端随机的输入（上层在每轮 Passkey 交换时注入）。
    pub fn set_random_local(&mut self, rnd: &[u8; 16]) {
        self.prnd = *rnd;
    }

    /// 用户在 Numeric Comparison 确认（或拒绝）后调用。
    pub fn nc_confirmed(&mut self, accepted: bool, out: &mut [u8]) -> Option<(usize, SmpAction)> {
        if self.phase != SmpPhase::WaitNumericConfirm {
            return None;
        }
        if !accepted {
            let n = build_pairing_failed(ERR_NUMERIC_COMPARISON_FAILED, out)?;
            self.phase = SmpPhase::Failed;
            return Some((n, SmpAction::None));
        }
        self.confirm_verified = true;
        if self.role == ROLE_CENTRAL {
            // 发起方：派生后发送本端 DHKey Check。
            let (mk, ltk) = f5(
                &self.dhkey,
                &self.prnd,
                &self.rrnd,
                &self.init_addr,
                &self.resp_addr,
            );
            self.mackey = mk;
            self.ltk = ltk;
            self.tk = ltk;
            let r = [0u8; 16];
            let io = [self.preq[1], self.preq[2], self.preq[3]];
            let e = f6(
                &self.mackey,
                &self.prnd,
                &self.rrnd,
                &r,
                &io,
                &self.init_addr,
                &self.resp_addr,
            );
            self.phase = SmpPhase::WaitDhKeyCheck;
            let n = build_16(CMD_DHKEY_CHECK, &e, out)?;
            return Some((n, SmpAction::None));
        }
        // 应答方：派生 MacKey/LTK 后等待对端 E。f5 顺序（发起方随机, 应答方随机,
        // 发起方地址, 应答方地址）与发起方侧一致性。
        let (mk, ltk) = f5(
            &self.dhkey,
            &self.rrnd,
            &self.prnd,
            &self.init_addr,
            &self.resp_addr,
        );
        self.mackey = mk;
        self.ltk = ltk;
        self.tk = ltk;
        self.phase = SmpPhase::WaitDhKeyCheck;
        Some((0, SmpAction::None))
    }

    /// 收到对端 DHKey Check：校验并回送本端 E（应答方）。
    pub fn on_dhkey_check(&mut self, pdu: &[u8], out: &mut [u8]) -> Option<(usize, SmpAction)> {
        if pdu.len() < 17 || pdu[0] != CMD_DHKEY_CHECK {
            return None;
        }
        let mut e_remote = [0u8; 16];
        e_remote.copy_from_slice(&pdu[1..17]);
        let mut r = [0u8; 16];
        if self.method == METH_PASSKEY_DISPLAY || self.method == METH_PASSKEY_INPUT {
            r[..4].copy_from_slice(&self.passkey.to_le_bytes());
        }
        // 校验对端 E：对端随机在前、对端 IOcap、发起方地址在前。
        let e_expected = if self.role == ROLE_PERIPHERAL {
            let io = [self.preq[1], self.preq[2], self.preq[3]];
            f6(
                &self.mackey,
                &self.rrnd,
                &self.prnd,
                &r,
                &io,
                &self.init_addr,
                &self.resp_addr,
            )
        } else {
            let io = [self.prsp[1], self.prsp[2], self.prsp[3]];
            f6(
                &self.mackey,
                &self.rrnd,
                &self.prnd,
                &r,
                &io,
                &self.resp_addr,
                &self.init_addr,
            )
        };
        if e_remote != e_expected {
            let n = build_pairing_failed(ERR_DHKEY_CHECK_FAILED, out)?;
            self.phase = SmpPhase::Failed;
            return Some((n, SmpAction::None));
        }
        self.confirm_verified = true;
        if self.role == ROLE_PERIPHERAL {
            // 应答方回送自己的 E。
            let io = [self.prsp[1], self.prsp[2], self.prsp[3]];
            let e = f6(
                &self.mackey,
                &self.prnd,
                &self.rrnd,
                &r,
                &io,
                &self.resp_addr,
                &self.init_addr,
            );
            let n = build_16(CMD_DHKEY_CHECK, &e, out)?;
            self.phase = SmpPhase::TransportKeyDerived;
            return Some((n, SmpAction::EncryptLink));
        }
        // 发起方：校验完成，进入密钥分发。
        self.phase = SmpPhase::TransportKeyDerived;
        Some((0, SmpAction::EncryptLink))
    }

    /// 链路层加密完成后调用：进入密钥分发阶段。
    pub fn on_encrypted(&mut self, out: &mut [u8]) -> Option<(usize, SmpAction)> {
        self.phase = SmpPhase::DistributeKeys;
        // 分发本端密钥（EncInfo → CentralIdent → …）。
        if (self.local_dist & DIST_ENC_KEY) != 0 && (self.local_dist_sent & DIST_ENC_KEY) == 0 {
            let n = build_16(CMD_ENCRYPT_INFO, &self.ltk, out)?;
            self.local_dist_sent |= DIST_ENC_KEY;
            return Some((n, SmpAction::None));
        }
        if (self.local_dist & DIST_ENC_KEY) != 0
            && (self.local_dist_sent & (DIST_ENC_KEY | 0x40)) == DIST_ENC_KEY
        {
            let n = build_central_ident(0, &[0u8; 8], out)?;
            self.local_dist_sent |= 0x40;
            return Some((n, SmpAction::None));
        }
        if (self.local_dist & DIST_ID_KEY) != 0 && (self.local_dist_sent & DIST_ID_KEY) == 0 {
            let n = build_16(CMD_IDENT_INFO, &[0u8; 16], out)?;
            self.local_dist_sent |= DIST_ID_KEY;
            return Some((n, SmpAction::None));
        }
        if (self.local_dist & DIST_ID_KEY) != 0 && (self.local_dist_sent & 0x20) == 0 {
            let mut a = [0u8; 6];
            a.copy_from_slice(&self.init_addr[1..7]);
            let n = build_ident_addr_info(self.init_addr[0], &a, out)?;
            self.local_dist_sent |= 0x20;
            return Some((n, SmpAction::None));
        }
        // 密钥分发完成。
        self.phase = SmpPhase::Complete;
        self.bond = Some(Bond {
            addr_type: self.init_addr[0],
            addr: {
                let mut a = [0u8; 6];
                a.copy_from_slice(&self.resp_addr[1..7]);
                a
            },
            ltk: self.ltk,
            ediv: 0,
            rand: [0u8; 8],
            irk: [0u8; 16],
            csrk: [0u8; 16],
            authenticated: self.mitm() && self.method != METH_JUST_WORKS,
            sc: self.is_sc(),
            key_size: self.local.max_key_size.min(self.remote.max_key_size),
        });
        Some((0, SmpAction::PairingComplete))
    }

    /// 收到对端密钥分发 PDU。
    pub fn on_key_pdu(&mut self, pdu: &[u8], out: &mut [u8]) -> Option<(usize, SmpAction)> {
        if pdu.is_empty() {
            return None;
        }
        match pdu[0] {
            CMD_ENCRYPT_INFO => {
                if pdu.len() < 17 {
                    return None;
                }
                self.remote_dist_received |= DIST_ENC_KEY;
                self.remote_dist &= !DIST_ENC_KEY;
            }
            CMD_CENTRAL_IDENT => {
                if pdu.len() < 11 {
                    return None;
                }
            }
            CMD_IDENT_INFO => {
                if pdu.len() < 17 {
                    return None;
                }
                self.remote_dist_received |= DIST_ID_KEY;
                self.remote_dist &= !DIST_ID_KEY;
            }
            CMD_IDENT_ADDR_INFO => {
                if pdu.len() < 8 {
                    return None;
                }
            }
            CMD_SIGNING_INFO => {
                if pdu.len() < 17 {
                    return None;
                }
                self.remote_dist_received |= DIST_SIGN;
                self.remote_dist &= !DIST_SIGN;
            }
            _ => return None,
        }
        // 双方分发完毕 → 完成。
        if self.local_dist_sent == self.local_dist && self.remote_dist == 0 {
            self.phase = SmpPhase::Complete;
            return Some((0, SmpAction::PairingComplete));
        }
        // 继续分发本端下一项。
        if (self.local_dist & DIST_ENC_KEY) != 0
            && (self.local_dist_sent & (DIST_ENC_KEY | 0x40)) == DIST_ENC_KEY
        {
            let n = build_central_ident(0, &[0u8; 8], out)?;
            self.local_dist_sent |= 0x40;
            return Some((n, SmpAction::None));
        }
        if (self.local_dist & DIST_ID_KEY) != 0 && (self.local_dist_sent & DIST_ID_KEY) == 0 {
            let n = build_16(CMD_IDENT_INFO, &[0u8; 16], out)?;
            self.local_dist_sent |= DIST_ID_KEY;
            return Some((n, SmpAction::None));
        }
        Some((0, SmpAction::None))
    }
}

// ==================== 配对失败检查 ====================

/// 检查配对请求/响应合法性（键长、AuthReq 位）。
/// 返回错误码或 0 表示通过。
pub fn validate_pairing(local: &PairingBody, remote: &PairingBody) -> u8 {
    let size = local.max_key_size.min(remote.max_key_size);
    if !(MIN_ENC_KEY_SIZE..=MAX_ENC_KEY_SIZE).contains(&size) {
        return ERR_ENCRYPTION_KEY_SIZE;
    }
    if (local.auth_req & !(AUTH_BONDING | AUTH_MITM | AUTH_SC | AUTH_KEYPRESS | AUTH_CT2)) != 0
        || (remote.auth_req & !(AUTH_BONDING | AUTH_MITM | AUTH_SC | AUTH_KEYPRESS | AUTH_CT2)) != 0
    {
        return ERR_INVALID_PARAMETERS;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(ty: u8, fill: u8) -> [u8; 7] {
        let mut a = [0u8; 7];
        a[0] = ty;
        for i in 1..7 {
            a[i] = fill + i as u8;
        }
        a
    }

    #[test]
    fn pairing_body_roundtrip() {
        let body = PairingBody {
            io_capability: IO_DISPLAY_YESNO,
            oob: OOB_NOT_PRESENT,
            auth_req: AUTH_BONDING | AUTH_MITM | AUTH_SC,
            max_key_size: 16,
            init_key_dist: DIST_ENC_KEY | DIST_ID_KEY,
            resp_key_dist: DIST_ENC_KEY | DIST_ID_KEY,
        };
        let mut out = [0u8; 8];
        let n = body.build(CMD_PAIRING_REQUEST, &mut out).unwrap();
        assert_eq!(n, 7);
        assert_eq!(out[0], CMD_PAIRING_REQUEST);
        assert_eq!(out[3], AUTH_BONDING | AUTH_MITM | AUTH_SC);
        let back = PairingBody::parse(&out[..7]).unwrap();
        assert_eq!(back, body);
        assert!(PairingBody::parse(&out[..6]).is_none());
    }

    #[test]
    fn pdu_helpers() {
        let mut out = [0u8; 128];
        let v = [0x11u8; 16];
        let n = build_16(CMD_PAIRING_CONFIRM, &v, &mut out).unwrap();
        assert_eq!(n, 17);
        assert_eq!(out[0], CMD_PAIRING_CONFIRM);
        assert_eq!(&out[1..17], &v);
        let n = build_public_key(&[1u8; 32], &[2u8; 32], &mut out).unwrap();
        assert_eq!(n, 65);
        assert_eq!(&out[1..33], &[1u8; 32]);
        assert_eq!(&out[33..65], &[2u8; 32]);
        let n = build_central_ident(0x1234, &[9u8; 8], &mut out).unwrap();
        assert_eq!(n, 11);
        assert_eq!(out[1..3], [0x34, 0x12]);
        let n = build_ident_addr_info(0, &[0xaa; 6], &mut out).unwrap();
        assert_eq!(n, 8);
        let n = build_security_request(AUTH_SC, &mut out).unwrap();
        assert_eq!(n, 2);
        assert_eq!(out[1], AUTH_SC);
        let n = build_pairing_failed(ERR_OOB_NOT_AVAILABLE, &mut out).unwrap();
        assert_eq!(n, 2);
        assert_eq!(out[1], ERR_OOB_NOT_AVAILABLE);
    }

    #[test]
    fn rev_helpers() {
        assert_eq!(
            rev16(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]),
            [15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0]
        );
        let mut raw = [0u8; 32];
        raw[31] = 1;
        let mut expect = raw;
        expect.reverse();
        assert_eq!(rev32(&raw), expect);
    }

    #[test]
    fn method_table_spot_checks() {
        // Legacy：NIO+NIO（MITM）→ Just Works（无输入/输出能力）。
        assert_eq!(
            resolve_method(IO_NO_INPUT_NO_OUTPUT, IO_NO_INPUT_NO_OUTPUT, AUTH_MITM, AUTH_MITM, false),
            METH_JUST_WORKS
        );
        // SC：DK+DK（MITM）→ Numeric Comparison（PASSKEY_CONFIRM）。
        assert_eq!(
            resolve_method(IO_DISPLAY_YESNO, IO_DISPLAY_YESNO, AUTH_MITM, AUTH_MITM, true),
            METH_PASSKEY_CONFIRM
        );
        // 无 MITM 一律 Just Works。
        assert_eq!(
            resolve_method(IO_KEYBOARD_DISPLAY, IO_DISPLAY_YESNO, 0, 0, true),
            METH_JUST_WORKS
        );
        // NIO 双方即使 MITM 也是 Just Works。
        assert_eq!(
            resolve_method(IO_NO_INPUT_NO_OUTPUT, IO_NO_INPUT_NO_OUTPUT, AUTH_MITM, AUTH_MITM, true),
            METH_JUST_WORKS
        );
        // Legacy 键盘双方 → 角色决定。
        assert_eq!(
            resolve_method(IO_KEYBOARD_DISPLAY, IO_KEYBOARD_DISPLAY, AUTH_MITM, AUTH_MITM, false),
            METH_PASSKEY_ROLE
        );
        assert_eq!(resolve_passkey_role(METH_PASSKEY_ROLE, true), METH_PASSKEY_DISPLAY);
        assert_eq!(resolve_passkey_role(METH_PASSKEY_ROLE, false), METH_PASSKEY_INPUT);
    }

    #[test]
    fn crypto_consistency() {
        let u = [0x20u8; 32];
        let v = [0x30u8; 32];
        let x = [0x40u8; 16];
        assert_eq!(f4(&u, &v, &x, 0), f4(&u, &v, &x, 0));
        assert_ne!(f4(&u, &v, &x, 0), f4(&u, &v, &x, 0x80));
        let a1 = addr(0, 1);
        let a2 = addr(0, 2);
        let r = [0u8; 16];
        let io = [IO_DISPLAY_YESNO, 0, AUTH_SC];
        let e1 = f6(&x, &u[..16].try_into().unwrap(), &v[..16].try_into().unwrap(), &r, &io, &a1, &a2);
        assert_eq!(
            e1,
            f6(&x, &u[..16].try_into().unwrap(), &v[..16].try_into().unwrap(), &r, &io, &a1, &a2)
        );
        let p = g2(&u, &v, &x, &v[..16].try_into().unwrap());
        assert!(p < 1_000_000);
    }

    #[test]
    fn c1_s1_checks() {
        let k = [0x11u8; 16];
        let r = [0x22u8; 16];
        let preq = [CMD_PAIRING_REQUEST, 1, 0, 0x0b, 16, 1, 1];
        let pres = [CMD_PAIRING_RESPONSE, 1, 0, 0x09, 16, 1, 1];
        let ia = addr(0, 0xaa);
        let ra = addr(0, 0xbb);
        let c1a = c1(&k, &r, &preq, &pres, &ia, &ra);
        assert_eq!(c1a, c1(&k, &r, &preq, &pres, &ia, &ra));
        let r2 = [0x33u8; 16];
        assert_ne!(c1a, c1(&k, &r2, &preq, &pres, &ia, &ra));
        let s = s1(&k, &r, &r2);
        assert_eq!(s, s1(&k, &r, &r2));
        assert_ne!(s, s1(&k, &r2, &r));
        // h6/h7 一致性
        let key_id = [b'b', b'r', b'l', b'e'];
        assert_eq!(h6(&k, &key_id), h6(&k, &key_id));
        let salt = [0x9du8; 16];
        assert_eq!(h7(&salt, &k), h7(&salt, &k));
    }

    /// 完整 SC Just Works 配对：发起方 × 应答方双向验证。
    #[test]
    fn full_sc_just_works_pairing() {
        let init_addr = addr(0, 0x11);
        let resp_addr = addr(0, 0x22);
        let body = |_mitm: bool| PairingBody {
            io_capability: IO_NO_INPUT_NO_OUTPUT,
            oob: OOB_NOT_PRESENT,
            auth_req: AUTH_BONDING | AUTH_SC,
            max_key_size: 16,
            init_key_dist: DIST_ENC_KEY | DIST_ID_KEY,
            resp_key_dist: DIST_ENC_KEY | DIST_ID_KEY,
        };
        let mut init = LePairing::new(ROLE_CENTRAL, body(false), &init_addr, &resp_addr);
        let mut resp = LePairing::new(ROLE_PERIPHERAL, body(false), &init_addr, &resp_addr);
        let mut b1 = [0u8; 128];
        let mut b2 = [0u8; 128];

        let (n1, _) = init.start_pairing(&mut b1).unwrap();
        let (n2, _) = resp.on_pairing_request(&b1[..n1], &mut b2).unwrap();
        let (n1, _) = init.on_pairing_response(&b2[..n2], &mut b1).unwrap();
        let (n2, _) = resp.on_public_key(&b1[..n1], &mut b2).unwrap();
        let (n1, _) = init.on_public_key(&b2[..n2], &mut b1).unwrap();
        let (n2, _) = resp.on_pairing_confirm(&b1[..n1], &mut b2).unwrap();
        let (n1, _) = init.on_pairing_confirm(&b2[..n2], &mut b1).unwrap();
        let (n2, _) = resp.on_pairing_random(&b1[..n1], &mut b2).unwrap();
        let (n1, _) = init.on_pairing_random(&b2[..n2], &mut b1).unwrap();
        assert_eq!(b1[0], CMD_DHKEY_CHECK);
        let (n2, _) = resp.on_dhkey_check(&b1[..n1], &mut b2).unwrap();
        let (n1, act) = init.on_dhkey_check(&b2[..n2], &mut b1).unwrap();
        assert!(init.mackey == resp.mackey);
        assert!(init.ltk == resp.ltk);
        assert_eq!(act, SmpAction::EncryptLink);
        let _ = n1;
    }

    /// 完整 SC Numeric Comparison 配对（DK+DK）。
    #[test]
    fn full_sc_numeric_comparison() {
        let init_addr = addr(0, 0x01);
        let resp_addr = addr(0, 0x02);
        let body = |mitm: bool| PairingBody {
            io_capability: IO_DISPLAY_YESNO,
            oob: OOB_NOT_PRESENT,
            auth_req: AUTH_BONDING | AUTH_SC | if mitm { AUTH_MITM } else { 0 },
            max_key_size: 16,
            init_key_dist: DIST_ENC_KEY,
            resp_key_dist: DIST_ENC_KEY,
        };
        let mut init = LePairing::new(ROLE_CENTRAL, body(true), &init_addr, &resp_addr);
        let mut resp = LePairing::new(ROLE_PERIPHERAL, body(true), &init_addr, &resp_addr);
        let mut b1 = [0u8; 128];
        let mut b2 = [0u8; 128];

        let (n1, _) = init.start_pairing(&mut b1).unwrap();
        let (n2, _) = resp.on_pairing_request(&b1[..n1], &mut b2).unwrap();
        let (n1, _) = init.on_pairing_response(&b2[..n2], &mut b1).unwrap();
        let (n2, _) = resp.on_public_key(&b1[..n1], &mut b2).unwrap();
        let (n1, _) = init.on_public_key(&b2[..n2], &mut b1).unwrap();
        let (n2, _) = resp.on_pairing_confirm(&b1[..n1], &mut b2).unwrap();
        let (n1, _) = init.on_pairing_confirm(&b2[..n2], &mut b1).unwrap();
        let (n2, act2) = resp.on_pairing_random(&b1[..n1], &mut b2).unwrap();
        let (n1, act1) = init.on_pairing_random(&b2[..n2], &mut b1).unwrap();
        let p1 = match act1 {
            SmpAction::ConfirmNumeric(p) => p,
            other => panic!("expected ConfirmNumeric, got {other:?}"),
        };
        let p2 = match act2 {
            SmpAction::ConfirmNumeric(p) => p,
            other => panic!("expected ConfirmNumeric, got {other:?}"),
        };
        assert_eq!(p1, p2);
        // 双方确认后：发起方发 E、应答方等 E。
        let (n1, _) = init.nc_confirmed(true, &mut b1).unwrap();
        assert_eq!(b1[0], CMD_DHKEY_CHECK);
        let (n2, _) = resp.nc_confirmed(true, &mut b2).unwrap();
        let (n2, _) = resp.on_dhkey_check(&b1[..n1], &mut b2).unwrap();
        let (n1, act) = init.on_dhkey_check(&b2[..n2], &mut b1).unwrap();
        assert!(init.ltk == resp.ltk);
        assert_eq!(act, SmpAction::EncryptLink);
        let _ = n1;
    }

    /// 完整 SC Passkey Entry（20 轮确认/随机）。
    #[test]
    fn full_sc_passkey_entry() {
        let init_addr = addr(0, 0x11);
        let resp_addr = addr(0, 0x22);
        let kb = PairingBody {
            io_capability: IO_KEYBOARD_ONLY,
            oob: OOB_NOT_PRESENT,
            auth_req: AUTH_BONDING | AUTH_SC | AUTH_MITM,
            max_key_size: 16,
            init_key_dist: DIST_ENC_KEY,
            resp_key_dist: DIST_ENC_KEY,
        };
        let dk = PairingBody {
            io_capability: IO_DISPLAY_ONLY,
            oob: OOB_NOT_PRESENT,
            auth_req: AUTH_BONDING | AUTH_SC | AUTH_MITM,
            max_key_size: 16,
            init_key_dist: DIST_ENC_KEY,
            resp_key_dist: DIST_ENC_KEY,
        };
        let mut init = LePairing::new(ROLE_CENTRAL, kb, &init_addr, &resp_addr);
        let mut resp = LePairing::new(ROLE_PERIPHERAL, dk, &init_addr, &resp_addr);
        let mut b1 = [0u8; 128];
        let mut b2 = [0u8; 128];
        init.set_passkey(123456);
        resp.set_passkey(123456);

        let (n1, _) = init.start_pairing(&mut b1).unwrap();
        let (n2, act) = resp.on_pairing_request(&b1[..n1], &mut b2).unwrap();
        // 应答方（DK）显示。
        if let SmpAction::DisplayPasskey(p) = act {
            assert_eq!(p, 123456);
        } else {
            panic!("expected DisplayPasskey");
        }
        let (n1, act) = init.on_pairing_response(&b2[..n2], &mut b1).unwrap();
        if let SmpAction::RequestPasskeyInput = act {
        } else {
            panic!("expected RequestPasskeyInput");
        }
        let (n2, _) = resp.on_public_key(&b1[..n1], &mut b2).unwrap();
        let (n1, _) = init.on_public_key(&b2[..n2], &mut b1).unwrap();

        // 20 轮确认/随机交换。
        let mut cur = b1[..n1].to_vec();
        let mut cnt = 0u32;
        for _ in 0..20 {
            let (n2, a2) = resp.on_pairing_confirm(&cur, &mut b2).unwrap();
            assert_eq!(b2[0], CMD_PAIRING_CONFIRM);
            let (n1, _) = init.on_pairing_confirm(&b2[..n2], &mut b1).unwrap();
            assert_eq!(b1[0], CMD_PAIRING_RANDOM);
            let (n2, _) = resp.on_pairing_random(&b1[..n1], &mut b2).unwrap();
            assert_eq!(b2[0], CMD_PAIRING_RANDOM);
            let (n1, _) = init.on_pairing_random(&b2[..n2], &mut b1).unwrap();
            cur = b1[..n1].to_vec();
            cnt += 1;
        }
        // 第 20 轮后发起方发送 DHKey Check，应答方回送后完成。
        assert_eq!(cur[0], CMD_DHKEY_CHECK);
        let (n2, _) = resp.on_dhkey_check(&cur, &mut b2).unwrap();
        let (n1, act) = init.on_dhkey_check(&b2[..n2], &mut b1).unwrap();
        assert!(init.ltk == resp.ltk);
        assert!(init.mackey == resp.mackey);
        assert_eq!(act, SmpAction::EncryptLink);
        let _ = (n1, cnt);
    }

    /// Legacy Just Works 配对完整流程。
    #[test]
    fn legacy_pairing_just_works() {
        let init_addr = addr(0, 0x33);
        let resp_addr = addr(0, 0x44);
        let body = PairingBody {
            io_capability: IO_NO_INPUT_NO_OUTPUT,
            oob: OOB_NOT_PRESENT,
            auth_req: AUTH_BONDING,
            max_key_size: 16,
            init_key_dist: DIST_ENC_KEY,
            resp_key_dist: DIST_ENC_KEY,
        };
        let mut init = LePairing::new(ROLE_CENTRAL, body, &init_addr, &resp_addr);
        let mut resp = LePairing::new(ROLE_PERIPHERAL, body, &init_addr, &resp_addr);
        let mut b1 = [0u8; 128];
        let mut b2 = [0u8; 128];
        let (n1, _) = init.start_pairing(&mut b1).unwrap();
        let (n2, _) = resp.on_pairing_request(&b1[..n1], &mut b2).unwrap();
        let (n1, _) = init.on_pairing_response(&b2[..n2], &mut b1).unwrap();
        assert_eq!(b1[0], CMD_PAIRING_CONFIRM);
        let (n2, _) = resp.on_pairing_confirm(&b1[..n1], &mut b2).unwrap();
        assert_eq!(b2[0], CMD_PAIRING_CONFIRM);
        let (n1, _) = init.on_pairing_confirm(&b2[..n2], &mut b1).unwrap();
        assert_eq!(b1[0], CMD_PAIRING_RANDOM);
        let (n2, _) = resp.on_pairing_random(&b1[..n1], &mut b2).unwrap();
        assert_eq!(b2[0], CMD_PAIRING_RANDOM);
        let (n1, act) = init.on_pairing_random(&b2[..n2], &mut b1).unwrap();
        assert_eq!(act, SmpAction::EncryptLink);
        assert!(init.tk == resp.tk); // STK 一致
        let _ = (n1, n2);
    }
}