//! CYW43455（Broadcom/Cypress）固件加载协议：SDIO 传输 + 固件/NVRAM 下载。
//!
//! Raspberry Pi 4（BCM2711）板载 CYW43455：Wi-Fi 走 SDIO（eMMC/SDHCI
//! 控制器，brcmfmac 后端），蓝牙走 UART（HCI）。
//!
//! 覆盖：
//! - SDIO 命令帧：CMD52（I/O R/W Direct）与 CMD53（I/O R/W Extended）、
//!   CRC-7（生成多项式 0x89）、R5 响应解析
//! - 回平面（backplane/SB）窗口访问辅助：32 位地址 → F1 窗口编码
//! - 固件下载流程：唤醒 → 设置 F1 块大小 → 上传代码镜像到 RAM →
//!   上传 NVRAM 至 RAM 尾部并写 RAMEND 指针 → 启用 F2 → 读回验证
//! - 固件镜像头部（lenient 解析）与上传计划（NV 地址/总量）
//!
//! 说明：CYW43455 固件 blob 为闭源二进制；本模块以“协议与流程”实现为主，
//! 具体寄存器偏移（如 SBSDIO_SB_ACCESS）依据 brcmfmac/bcm SDIO 核定义，
//! 真机对齐见 docs/wireless.md。纯函数、无堆、绝不 panic。

// ==================== SDIO 传输原语 ====================

/// SD CRC-7（生成多项式 0x89，无初始异或，不取反）。
pub fn crc7(data: &[u8]) -> u8 {
    let mut crc: u8 = 0;
    for &b in data {
        let mut cur = b;
        for _ in 0..8 {
            let bit = (crc >> 6) ^ (cur >> 7);
            crc <<= 1;
            crc &= 0x7F;
            if bit != 0 {
                crc ^= 0x09;
            }
            cur <<= 1;
        }
    }
    crc
}

/// CMD52/CMD53 命令索引。
pub const CMD52_INDEX: u8 = 52;
pub const CMD53_INDEX: u8 = 53;

fn build_command_token(index: u8, arg: u32, out: &mut [u8]) -> Option<usize> {
    if out.len() < 6 {
        return None;
    }
    out[0] = index; // start(0) | transmit(0) | index(6)
    out[1..5].copy_from_slice(&arg.to_be_bytes());
    let crc = crc7(&out[..5]);
    out[5] = (crc << 1) | 1; // crc7(7) + end_bit(1)
    Some(6)
}

/// CMD52 参数：write(31) | func(30..28) | raw(26) | reg(25..9) | data(8..0)。
pub fn build_cmd52_arg(write: bool, func: u8, raw: bool, reg: u32, data: u8) -> u32 {
    let mut a: u32 = 0;
    if write {
        a |= 1 << 31;
    }
    a |= ((func & 7) as u32) << 28;
    if raw {
        a |= 1 << 26;
    }
    a |= (reg & 0x1FFFF) << 9;
    a |= data as u32;
    a
}

pub fn build_cmd52(write: bool, func: u8, raw: bool, reg: u32, data: u8, out: &mut [u8]) -> Option<usize> {
    let arg = build_cmd52_arg(write, func, raw, reg, data);
    build_command_token(CMD52_INDEX, arg, out)
}

/// CMD53 参数：write(31) | func(30..28) | block_mode(27) | op_write(26) |
/// reg(25..9) | count(8..0)。
pub fn build_cmd53(
    write: bool,
    func: u8,
    block_mode: bool,
    reg: u32,
    count: u32,
    out: &mut [u8],
) -> Option<usize> {
    let mut a: u32 = 0;
    if write {
        a |= 1 << 31;
    }
    a |= ((func & 7) as u32) << 28;
    if block_mode {
        a |= 1 << 27;
    }
    if write {
        a |= 1 << 26;
    }
    a |= (reg & 0x1FFFF) << 9;
    a |= count & 0x1FF;
    build_command_token(CMD53_INDEX, a, out)
}

/// 解析 R5 响应（6 字节 BE）：返回 `(status, data)`。
/// bits：data(47..40) | status(23..16) | crc7(7..1) | end(0)。
pub fn parse_r5(resp: &[u8]) -> Option<(u16, u8)> {
    if resp.len() < 6 {
        return None;
    }
    let data = resp[0];
    let status = u16::from_be_bytes([resp[2], resp[3]]) & 0xFF00;
    let crc = resp[5] >> 1;
    if crc != crc7(&resp[..5]) {
        return None;
    }
    Some((status, data))
}

// ==================== 回平面窗口辅助 ====================

/// F1 背板访问寄存器（brcmfmac/bcm SDIO 核定义，真机需按 CYW43455 手册核对）。
pub const SBSDIO_SB_ACCESS: u32 = 0x48;
pub const SBSDIO_SB_WINDOW_ADDR: u32 = 0x50;
pub const SBSDIO_SB_WINDOW_DATA: u32 = 0x54;
pub const SBSDIO_FUNC1_WAKE: u32 = 0x74;
pub const SBSDIO_FUNC1_CHIPCLKCSR: u32 = 0x6C;

/// 把 32 位背板地址编码为窗口控制字（2 字节，写入 SBSDIO_SB_WINDOW_ADDR）。
pub fn window_addr(host_addr: u32) -> [u8; 2] {
    [((host_addr >> 24) & 0xFF) as u8, ((host_addr >> 16) & 0xFF) as u8]
}

/// 16KB 窗口内相对偏移（低 14 位）。
pub fn window_offset(host_addr: u32) -> u32 {
    host_addr & 0x3FFF
}

// ==================== 固件镜像 ====================

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FwImage {
    pub raw_len: usize,
    /// 上传到 RAM 的起始地址（常规 0）。
    pub load_addr: u32,
}

/// 解析固件 blob：返回镜像（整块上传）。固件头部字段不被本库解释，
/// 由驱动按 blob 头部先写镜像再写 NVRAM 的标准流程执行。
pub fn parse_fw_image(raw: &[u8]) -> Option<FwImage> {
    if raw.is_empty() {
        return None;
    }
    Some(FwImage {
        raw_len: raw.len(),
        load_addr: 0,
    })
}

/// 上传计划：返回 `(镜像基址, NVRAM 起始, NVRAM 结束)`。
/// 镜像放 RAM 起始，NVRAM 放末端。
pub fn upload_plan(fw: &FwImage, nvram: &[u8], ramsize: u32) -> Option<(u32, u32, u32)> {
    if fw.raw_len as u64 + nvram.len() as u64 > ramsize as u64 {
        return None;
    }
    let base = fw.load_addr;
    let nv_start = ramsize - nvram.len() as u32;
    let nv_end = ramsize;
    Some((base, nv_start, nv_end))
}

/// NVRAM 地址指针位置：简化模型 `(pointer_addr, pointer_value)`。
pub fn nvram_pointer(nv_start: u32) -> (u32, u32) {
    (nv_start, nv_start)
}

// ==================== 下载流程状态机 ====================

pub const FW_STEP_WAKE: u8 = 0;
pub const FW_STEP_BLOCK_SIZE: u8 = 1;
pub const FW_STEP_CODE: u8 = 2;
pub const FW_STEP_NVRAM: u8 = 3;
pub const FW_STEP_RAMEND: u8 = 4;
pub const FW_STEP_ENABLE_F2: u8 = 5;
pub const FW_STEP_VERIFY: u8 = 6;

/// 固件下载状态机：
/// 1) 唤醒 F1（写 SBSDIO_FUNC1_WAKE）并读取 chipclkcsr；
/// 2) 设置 F1 块大小（SDIO I/O 块，64/128/256…）；
/// 3) 通过背板窗口把代码镜像整块写入 RAM 起始；
/// 4) NVRAM 写入 RAM 末尾；
/// 5) 写 RAMEND 指针；
/// 6) 启用 F2（数据通道）并让固件运行；
/// 7) 读回设备信息验证。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FwLoader {
    pub step: u8,
    pub fw_len: usize,
    pub nvram_len: usize,
    pub ramsize: u32,
    pub nv_addr: u32,
    pub code_done: bool,
    pub nv_done: bool,
}

impl FwLoader {
    pub fn new(fw: &FwImage, nvram: &[u8], ramsize: u32) -> Option<Self> {
        let plan = upload_plan(fw, nvram, ramsize)?;
        Some(FwLoader {
            step: FW_STEP_WAKE,
            fw_len: fw.raw_len,
            nvram_len: nvram.len(),
            ramsize,
            nv_addr: plan.1,
            code_done: false,
            nv_done: false,
        })
    }

    /// 推进状态机：返回下一步动作。
    pub fn next(&mut self) -> u8 {
        let s = self.step;
        match s {
            FW_STEP_WAKE => self.step = FW_STEP_BLOCK_SIZE,
            FW_STEP_BLOCK_SIZE => self.step = FW_STEP_CODE,
            FW_STEP_CODE => {
                self.code_done = true;
                self.step = FW_STEP_NVRAM;
            }
            FW_STEP_NVRAM => {
                self.nv_done = true;
                self.step = FW_STEP_RAMEND;
            }
            FW_STEP_RAMEND => self.step = FW_STEP_ENABLE_F2,
            FW_STEP_ENABLE_F2 => self.step = FW_STEP_VERIFY,
            FW_STEP_VERIFY => self.step = FW_STEP_VERIFY,
            _ => {}
        }
        s
    }

    pub fn done(&self) -> bool {
        self.step == FW_STEP_VERIFY && self.code_done && self.nv_done
    }
}

/// 把镜像切分为 16KB 窗口块：返回 `(窗口基数, 块内起始偏移)` 数量。
pub fn window_chunks(len: usize, out: &mut [(u32, u32); 8]) -> usize {
    let mut n = 0usize;
    let mut remaining = len;
    let mut addr = 0u32;
    while remaining > 0 && n < 8 {
        let win = (addr / 0x4000) * 0x4000;
        let off = addr % 0x4000;
        let space = 0x4000 - off;
        let take = remaining.min(space as usize);
        out[n] = (win, off);
        n += 1;
        addr = addr.wrapping_add(take as u32);
        remaining -= take;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc7_known_vector() {
        // SD CRC-7（poly 0x09 反射 0x89）对 "123456789" 的校验值为 0x75。
        assert_eq!(crc7(b"123456789"), 0x75);
        assert_eq!(crc7(b""), 0x00);
    }

    #[test]
    fn cmd52_cmd53_framing() {
        let mut out = [0u8; 8];
        // CMD52 读（func1, reg 0x48, 非 raw）
        let n = build_cmd52(false, 1, false, 0x48, 0, &mut out).unwrap();
        assert_eq!(n, 6);
        assert_eq!(out[0], CMD52_INDEX);
        // arg = func<<28 | reg<<9 → 0x1000_9000
        assert_eq!(out[1], 0x10);
        assert_eq!(out[2], 0x00);
        assert_eq!(out[3], 0x90);
        assert_eq!(out[4], 0x00);
        assert_eq!(out[5] & 1, 1);
        assert_eq!(crc7(&out[..5]), out[5] >> 1);
        // CMD53 写块（func1, reg 0x0, 块模式, 32 块）
        let n = build_cmd53(true, 1, true, 0x00, 32, &mut out).unwrap();
        assert_eq!(n, 6);
        assert_eq!(out[0], CMD53_INDEX);
        // write(31) | func(28) | block(27) | write op(26) → 0x9C
        assert_eq!(out[1] & 0xF0, 0x90);
        assert_eq!(out[2], 0);
        assert_eq!(out[3], 0);
        assert_eq!(out[4], 32); // count 低 9 位在 arg 末尾
        assert_eq!(out[5] >> 1, crc7(&out[..5]));
    }

    #[test]
    fn r5_parse() {
        let mut resp = [0u8; 6];
        resp[0] = 0xAA; // data
        resp[2] = 0x00;
        resp[3] = 0x00;
        resp[4] = 0x00;
        resp[5] = (crc7(&resp[..5]) << 1) | 1;
        let (status, data) = parse_r5(&resp).unwrap();
        assert_eq!(data, 0xAA);
        assert_eq!(status, 0);
        resp[5] ^= 0x02; // 坏 CRC
        assert!(parse_r5(&resp).is_none());
    }

    #[test]
    fn window_math() {
        // 镜像 0x9000 字节 → 两个 16KB 窗口块
        let mut chunks = [(0u32, 0u32); 8];
        let n = window_chunks(0x9000, &mut chunks);
        // 0x9000 = 36864 字节 = 两个完整 16KB 窗口 + 一个 4KB 尾部
        assert_eq!(n, 3);
        assert_eq!(chunks[0], (0, 0));
        assert_eq!(chunks[1], (0x4000, 0));
        assert_eq!(chunks[2], (0x8000, 0));
        assert_eq!(window_addr(0x1800_0000), [0x18, 0x00]);
        assert_eq!(window_offset(0x1800_1234 & 0x3FFF), 0x1234);
    }

    #[test]
    fn upload_plan_and_loader() {
        let fw = parse_fw_image(&[0xAAu8; 8192]).unwrap();
        let nvram = b"macaddr=00:11:22:33:44:55\n";
        let loader = FwLoader::new(&fw, nvram, 0x1F_0000).unwrap();
        assert_eq!(loader.nv_addr, 0x1F_0000 - nvram.len() as u32);
        let mut l = loader;
        assert_eq!(l.next(), FW_STEP_WAKE);
        assert_eq!(l.next(), FW_STEP_BLOCK_SIZE);
        assert_eq!(l.next(), FW_STEP_CODE);
        assert!(l.code_done);
        assert_eq!(l.next(), FW_STEP_NVRAM);
        assert!(l.nv_done);
        assert_eq!(l.next(), FW_STEP_RAMEND);
        assert_eq!(l.next(), FW_STEP_ENABLE_F2);
        assert_eq!(l.next(), FW_STEP_VERIFY);
        assert!(l.done());
    }
}