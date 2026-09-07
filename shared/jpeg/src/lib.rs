//! KunxOS 纯 Rust baseline JPEG 编码器。
//!
//! - 输入：RGB888（行主序，`w*h*3` 字节），JPEG 标准 BT.601 色彩空间
//! - 输出：JFIF baseline JPEG，4:2:0 色度抽样，8x8 DCT + 标准量化表/Huffman 表
//! - 约束：`#![no_std]`、零堆分配、纯整数运算（无浮点），可在 EL0 裸 syscall
//!   环境（camerad）与宿主测试环境（`cargo test`）下运行。
//!
//! 参考：ITU-T T.81（JPEG 规范）、IJG jpeglib 标准表（Annex K）。
//! 单元测试在宿主架构运行（结构校验 + 重编码一致性）；视觉校验见
//! `tools/verify_jpeg.py`（有 Pillow 时解码对比，无则做标记结构检查）。

#![no_std]

#[cfg(test)]
extern crate std;

// ==================== 公共 API ====================

/// 编码错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JpegError {
    /// 输出缓冲不足；`needed` 为本次编码实际需要的字节数估计。
    BufferTooSmall { needed: usize },
    /// 宽或高为 0，或像素数据长度与 `w*h*3` 不符。
    BadDimensions,
    /// quality 超出 1..=100。
    BadQuality,
}

/// RGB888 → baseline JPEG（JFIF，4:2:0）。
///
/// 写入 `dst`，返回编码后的字节数。宽高可为奇数（边缘像素复制补齐）。
pub fn jpeg_encode_rgb888(
    dst: &mut [u8],
    width: usize,
    height: usize,
    rgb: &[u8],
    quality: u8,
) -> Result<usize, JpegError> {
    if width == 0 || height == 0 {
        return Err(JpegError::BadDimensions);
    }
    if rgb.len() != width * height * 3 {
        return Err(JpegError::BadDimensions);
    }
    if quality == 0 || quality > 100 {
        return Err(JpegError::BadQuality);
    }

    let mut w = Writer::new(dst);

    // ---- 头部（标记段：0xFF 不做字节填充） ----
    w.marker(0xD8); // SOI

    // APP0：JFIF 1.01
    w.marker(0xE0);
    w.u16(16);
    w.bytes(b"JFIF\0");
    w.byte(1); // 版本主
    w.byte(1); // 版本次
    w.byte(0); // units：无（宽高比 1:1）
    w.u16(1); // X density
    w.u16(1); // Y density
    w.byte(0); // 缩略图宽
    w.byte(0); // 缩略图高

    // DQT：亮度表 0 + 色度表 1（按 quality 缩放）
    let (qy, qc) = quant_tables(quality);
    w.marker(0xDB);
    w.u16(132); // 2 + (1+64) × 2
    w.byte(0); // 表 0（亮度）
    for &q in &qy {
        w.byte(q);
    }
    w.byte(1); // 表 1（色度）
    for &q in &qc {
        w.byte(q);
    }

    // SOF0：baseline，8bit，3 分量，Y 4:2:0
    w.marker(0xC0);
    w.u16(17);
    w.byte(8); // 精度
    w.u16(height as u16);
    w.u16(width as u16);
    w.byte(3); // 分量数
    w.byte(1); // 分量 1：Y
    w.byte(0x22); // 水平 2 垂直 2 抽样
    w.byte(0); // 量化表 0
    w.byte(2); // 分量 2：Cb
    w.byte(0x11);
    w.byte(1); // 量化表 1
    w.byte(3); // 分量 3：Cr
    w.byte(0x11);
    w.byte(1);

    // DHT：DC0 / AC0 / DC1 / AC1 四张标准表
    write_dht(&mut w, 0, &DC_LUM_BITS, &DC_LUM_VALS);
    write_dht(&mut w, 0x10, &AC_LUM_BITS, &AC_LUM_VALS);
    write_dht(&mut w, 1, &DC_CHR_BITS, &DC_CHR_VALS);
    write_dht(&mut w, 0x11, &AC_CHR_BITS, &AC_CHR_VALS);

    // SOS
    w.marker(0xDA);
    w.u16(12);
    w.byte(3);
    w.byte(1);
    w.byte(0x00); // Y：DC 表 0，AC 表 0
    w.byte(2);
    w.byte(0x11); // Cb：DC 表 1，AC 表 1
    w.byte(3);
    w.byte(0x11); // Cr：DC 表 1，AC 表 1
    w.byte(0);
    w.byte(63);
    w.byte(0);

    // ---- 熵编码（数据段：0xFF 必须做字节填充） ----
    let huff_dc0 = build_huff(&DC_LUM_BITS, &DC_LUM_VALS);
    let huff_ac0 = build_huff(&AC_LUM_BITS, &AC_LUM_VALS);
    let huff_dc1 = build_huff(&DC_CHR_BITS, &DC_CHR_VALS);
    let huff_ac1 = build_huff(&AC_CHR_BITS, &AC_CHR_VALS);

    let mcu_w = (width + 15) / 16;
    let mcu_h = (height + 15) / 16;

    let mut prev_dc = [0i32; 3]; // Y / Cb / Cr
    let mut yblock: [i32; 64] = [0; 64];
    let mut dct: [i32; 64] = [0; 64];

    for my in 0..mcu_h {
        for mx in 0..mcu_w {
            let px0 = mx * 16;
            let py0 = my * 16;

            // 1. 采集 16x16 采样块：Y 平面 + Cb/Cr 2x2 平均
            let mut yplane = [0i32; 16 * 16];
            let mut cb = [0i32; 64];
            let mut cr = [0i32; 64];
            for cy in 0..8 {
                for cx in 0..8 {
                    let mut sb = 0i32;
                    let mut sr = 0i32;
                    for dy in 0..2 {
                        let py = clamp_y(py0 + cy * 2 + dy, height);
                        for dx in 0..2 {
                            let px = clamp_x(px0 + cx * 2 + dx, width);
                            let idx = (py * width + px) * 3;
                            let (r, g, b) = (rgb[idx] as i32, rgb[idx + 1] as i32, rgb[idx + 2] as i32);
                            let y = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
                            yplane[(cy * 2 + dy) * 16 + cx * 2 + dx] = y;
                            sb += ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                            sr += ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
                        }
                    }
                    cb[cy * 8 + cx] = sb >> 2;
                    cr[cy * 8 + cx] = sr >> 2;
                }
            }

            // 2. 编码 6 个块：Y(0,0) Y(8,0) Y(0,8) Y(8,8) Cb Cr
            let y_offsets = [0usize, 8, 16 * 8, 16 * 8 + 8];
            for &off in &y_offsets {
                for i in 0..8 {
                    for j in 0..8 {
                        yblock[i * 8 + j] = yplane[off + i * 16 + j] - 128;
                    }
                }
                encode_block(&mut w, &mut dct, &yblock, &huff_dc0, &huff_ac0, &qy, &mut prev_dc[0]);
            }
            for i in 0..8 {
                for j in 0..8 {
                    yblock[i * 8 + j] = cb[i * 8 + j] - 128;
                }
            }
            encode_block(&mut w, &mut dct, &yblock, &huff_dc1, &huff_ac1, &qc, &mut prev_dc[1]);
            for i in 0..8 {
                for j in 0..8 {
                    yblock[i * 8 + j] = cr[i * 8 + j] - 128;
                }
            }
            encode_block(&mut w, &mut dct, &yblock, &huff_dc1, &huff_ac1, &qc, &mut prev_dc[2]);
        }
    }

    w.finish()?;

    // ---- 结束 ----
    w.marker(0xD9); // EOI

    if w.overflow {
        return Err(JpegError::BufferTooSmall {
            needed: w.needed_estimate(),
        });
    }
    Ok(w.pos)
}

fn clamp_x(x: usize, w: usize) -> usize {
    if x >= w {
        w - 1
    } else {
        x
    }
}
fn clamp_y(y: usize, h: usize) -> usize {
    if y >= h {
        h - 1
    } else {
        y
    }
}

// ==================== 单块编码 ====================

/// 编码一个 8x8 块：FDCT（整数）→ 量化 → zigzag → Huffman（DC 差分 + AC 行程）。
fn encode_block(
    w: &mut Writer,
    dct: &mut [i32; 64],
    block: &[i32; 64],
    hdc: &HuffTable,
    hac: &HuffTable,
    q: &[u8; 64],
    prev_dc: &mut i32,
) {
    fdct_i32(block, dct);

    // 量化 + zigzag 重排；round-half-away-from-zero（IJG 语义）
    let mut qz = [0i32; 64];
    for (i, &z) in ZIGZAG.iter().enumerate() {
        let v = dct[z];
        let qv = q[z] as i32;
        let r = if v >= 0 {
            (v + qv / 2) / qv
        } else {
            -(((-v) + qv / 2) / qv)
        };
        qz[i] = r.clamp(-32768, 32767);
    }

    // DC 差分
    let dc = qz[0];
    let diff = dc - *prev_dc;
    *prev_dc = dc;
    let cat = magnitude_cat(diff);
    w.huff(hdc, cat as usize);
    w.magnitude(diff, cat);

    // AC：行程编码
    let mut run = 0usize;
    for &coef in &qz[1..] {
        if coef == 0 {
            run += 1;
            continue;
        }
        while run >= 16 {
            w.huff(hac, 0xF0); // ZRL
            run -= 16;
        }
        let c = magnitude_cat(coef);
        let sym = (run << 4) | c as usize;
        w.huff(hac, sym);
        w.magnitude(coef, c);
        run = 0;
    }
    if run > 0 {
        w.huff(hac, 0x00); // EOB
    }
}

/// 8x8 整数 FDCT（可分离，行变换 + 列变换）。
///
/// 一维基函数：F[k] = (1/2)·C(k)·Σ_x f[x]·cos((2x+1)kπ/16)，C(0)=1/√2。
/// 两趟后整体为 (1/4)·C(u)·C(v)·ΣΣ，与 JPEG 标准正向 DCT 约定一致
/// （解码端按同约定重建，亮度/色度幅度正确）。
fn fdct_i32(src: &[i32; 64], dst: &mut [i32; 64]) {
    let mut tmp = [0i32; 64];
    for i in 0..8 {
        fdct1d(&src[i * 8..i * 8 + 8], &mut tmp[i * 8..i * 8 + 8]);
    }
    let mut col = [0i32; 8];
    for j in 0..8 {
        let mut input = [0i32; 8];
        for i in 0..8 {
            input[i] = tmp[i * 8 + j];
        }
        fdct1d(&input, &mut col);
        for i in 0..8 {
            dst[i * 8 + j] = col[i];
        }
    }
}

/// 8 点一维 DCT：系数 cos((2x+1)kπ/16)×16384 查表。
///
/// 计算 F[k] = (1/2)·C(k)·Σ_x f[x]·cos((2x+1)kπ/16)，C(0)=1/√2：
/// `s = Σ f·cos·16384`，`norm = C(k)·16384`，故
/// `F[k] = s·norm / (2·16384·16384)`，用「加半后算术右移 29 位」实现取整。
/// 两趟（行 + 列）后整体为 (1/4)·C(u)·C(v)·ΣΣ，与 JPEG 标准正向 DCT 约定一致。
fn fdct1d(src: &[i32], dst: &mut [i32]) {
    const T: [[i32; 8]; 8] = [
        [16384, 16384, 16384, 16384, 16384, 16384, 16384, 16384],
        [16069, 13623, 9102, 3196, -3196, -9102, -13623, -16069],
        [15137, 6270, -6270, -15137, -15137, -6270, 6270, 15137],
        [13623, -3196, -16069, -9102, 9102, 16069, 3196, -13623],
        [11585, -11585, -11585, 11585, 11585, -11585, -11585, 11585],
        [9102, -16069, 3196, 13623, -13623, -3196, 16069, -9102],
        [6270, -15137, 15137, -6270, -6270, 15137, -15137, 6270],
        [3196, -9102, 13623, -16069, 16069, -13623, 9102, -3196],
    ];
    for k in 0..8 {
        let mut s: i64 = 0;
        for x in 0..8 {
            s += (src[x] as i64) * (T[k][x] as i64);
        }
        // C(0) = 1/√2 ≈ 11585/16384；最终 /(2·16384·16384)，加半取整
        let norm = if k == 0 { 11585 } else { 16384 };
        let num = s * norm;
        dst[k] = ((num + (1i64 << 28)) >> 29) as i32;
    }
}

/// 幅值所需的位数（JPEG DC/AC 分类）。
fn magnitude_cat(v: i32) -> u8 {
    let mut n = 0u8;
    let mut x = v.unsigned_abs();
    while x > 0 {
        n += 1;
        x >>= 1;
    }
    n
}

// ==================== Huffman ====================

struct HuffTable {
    codes: [u16; 256],
    sizes: [u8; 256],
}

/// 由标准位长表 + 值表构造规范 Huffman 表。
///
/// `bits[1..=16]` 为各码长的符号数，`vals` 按码长分组顺序排列
/// （标准表即如此），按组递增分配规范码：组内 +1，组间左移 1 位。
fn build_huff(bits: &[u8; 17], vals: &[u8]) -> HuffTable {
    let mut t = HuffTable {
        codes: [0; 256],
        sizes: [0; 256],
    };
    let mut code: u32 = 0;
    let mut vi = 0usize;
    for len in 1..=16usize {
        let count = bits[len] as usize;
        for _ in 0..count {
            let v = vals[vi] as usize;
            vi += 1;
            debug_assert!(vi <= vals.len());
            t.sizes[v] = len as u8;
            t.codes[v] = code as u16;
            code += 1;
        }
        code <<= 1;
    }
    debug_assert_eq!(vi, vals.len(), "标准表位长计数与值数不符");
    t
}

// ==================== 位写入器 ====================

struct Writer<'a> {
    out: &'a mut [u8],
    pos: usize,
    acc: u32,
    nbits: u8,
    overflow: bool,
}

impl<'a> Writer<'a> {
    fn new(out: &'a mut [u8]) -> Self {
        Writer {
            out,
            pos: 0,
            acc: 0,
            nbits: 0,
            overflow: false,
        }
    }

    /// 写原始字节（头部段内使用，不做 0xFF 填充）。
    fn byte(&mut self, b: u8) {
        if self.pos >= self.out.len() {
            self.overflow = true;
            return;
        }
        self.out[self.pos] = b;
        self.pos += 1;
    }

    /// 写标记：0xFF + 标记码（不填充）。
    fn marker(&mut self, m: u8) {
        self.byte(0xFF);
        self.byte(m);
    }

    fn bytes(&mut self, s: &[u8]) {
        for &b in s {
            self.byte(b);
        }
    }

    fn u16(&mut self, v: u16) {
        self.byte((v >> 8) as u8);
        self.byte(v as u8);
    }

    /// 写 Huffman 码字（熵数据，走字节填充路径）。
    fn huff(&mut self, t: &HuffTable, sym: usize) {
        let len = t.sizes[sym] as u32;
        let code = t.codes[sym] as u32;
        self.put_bits(code, len);
    }

    /// 写幅值位（`cat` 位，MSB 优先）。
    fn magnitude(&mut self, v: i32, cat: u8) {
        if cat == 0 {
            return;
        }
        let bits = if v >= 0 {
            v as u32
        } else {
            (v + (1i32 << cat) - 1) as u32
        };
        self.put_bits(bits, cat as u32);
    }

    /// 写 `count` 位（MSB 优先）；落盘字节做 0xFF→0xFF 0x00 填充。
    fn put_bits(&mut self, bits: u32, count: u32) {
        if count == 0 {
            return;
        }
        self.acc = (self.acc << count) | (bits & ((1u32 << count) - 1));
        self.nbits += count as u8;
        while self.nbits >= 8 {
            let b = ((self.acc >> (self.nbits - 8)) & 0xFF) as u8;
            self.nbits -= 8;
            // 熵数据：0xFF 后补 0x00
            if b == 0xFF {
                if self.pos >= self.out.len() {
                    self.overflow = true;
                    return;
                }
                self.out[self.pos] = 0xFF;
                self.pos += 1;
                if self.pos >= self.out.len() {
                    self.overflow = true;
                    return;
                }
                self.out[self.pos] = 0x00;
                self.pos += 1;
            } else {
                if self.pos >= self.out.len() {
                    self.overflow = true;
                    return;
                }
                self.out[self.pos] = b;
                self.pos += 1;
            }
        }
    }

    /// 字节对齐：剩余位补 1。
    fn finish(&mut self) -> Result<(), JpegError> {
        if self.nbits > 0 {
            let pad = (1u32 << (8 - self.nbits)) - 1;
            self.put_bits(pad, (8 - self.nbits) as u32);
        }
        if self.overflow {
            return Err(JpegError::BufferTooSmall {
                needed: self.needed_estimate(),
            });
        }
        Ok(())
    }

    fn needed_estimate(&self) -> usize {
        self.pos + ((self.nbits as usize + 7) / 8) + 4096
    }
}

// ==================== 标准表 ====================

/// zigzag 顺序（JPEG Annex F.1.2.2）。
const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// 标准亮度量化表（Annex K.1，quality=50 基准）。
const Q_LUM: [u8; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69,
    56, 14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104,
    113, 92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];

/// 标准色度量化表（Annex K.1）。
const Q_CHR: [u8; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99,
    99, 47, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
];

/// 按 quality（1..100）缩放量化表（IJG 公式）。
fn quant_tables(quality: u8) -> ([u8; 64], [u8; 64]) {
    let q = if quality < 50 {
        (5000 / quality as i32).max(1)
    } else {
        (200 - quality as i32 * 2).max(1)
    };
    let mut y = [0u8; 64];
    let mut c = [0u8; 64];
    for i in 0..64 {
        let vy = (Q_LUM[i] as i32 * q + 50) / 100;
        let vc = (Q_CHR[i] as i32 * q + 50) / 100;
        y[i] = vy.clamp(1, 255) as u8;
        c[i] = vc.clamp(1, 255) as u8;
    }
    (y, c)
}

/// 标准 DC 亮度表（Annex K.3.3）：bits[1..16]，共 12 个值。
const DC_LUM_BITS: [u8; 17] = [0, 0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
const DC_LUM_VALS: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];

/// 标准 DC 色度表（Annex K.3.3）。
const DC_CHR_BITS: [u8; 17] = [0, 0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
const DC_CHR_VALS: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];

/// 标准 AC 亮度表（Annex K.3.3，162 值）。
const AC_LUM_BITS: [u8; 17] = [0, 0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7D];
const AC_LUM_VALS: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61,
    0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xA1, 0x08, 0x23, 0x42, 0xB1, 0xC1, 0x15, 0x52,
    0xD1, 0xF0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0A, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x25, 0x26,
    0x27, 0x28, 0x29, 0x2A, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3A, 0x43, 0x44, 0x45, 0x46, 0x47,
    0x48, 0x49, 0x4A, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A, 0x63, 0x64, 0x65, 0x66, 0x67,
    0x68, 0x69, 0x6A, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x83, 0x84, 0x85, 0x86, 0x87,
    0x88, 0x89, 0x8A, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5,
    0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xC2,
    0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7, 0xD8,
    0xD9, 0xDA, 0xE1, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA, 0xF1, 0xF2, 0xF3,
    0xF4, 0xF5, 0xF6, 0xF7, 0xF8, 0xF9, 0xFA,
];

/// 标准 AC 色度表（Annex K.3.3，162 值）。
const AC_CHR_BITS: [u8; 17] = [0, 0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];
const AC_CHR_VALS: [u8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61,
    0x71, 0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xA1, 0xB1, 0xC1, 0x09, 0x23, 0x33, 0x52,
    0xF0, 0x15, 0x62, 0x72, 0xD1, 0x0A, 0x16, 0x24, 0x34, 0xE1, 0x25, 0xF1, 0x17, 0x18, 0x19, 0x1A,
    0x26, 0x27, 0x28, 0x29, 0x2A, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3A, 0x43, 0x44, 0x45, 0x46, 0x47,
    0x48, 0x49, 0x4A, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A, 0x63, 0x64, 0x65, 0x66, 0x67,
    0x68, 0x69, 0x6A, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x82, 0x83, 0x84, 0x85, 0x86,
    0x87, 0x88, 0x89, 0x8A, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3, 0xA4,
    0xA5, 0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA,
    0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7,
    0xD8, 0xD9, 0xDA, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA, 0xF2, 0xF3, 0xF4,
    0xF5, 0xF6, 0xF7, 0xF8, 0xF9, 0xFA,
];

/// 写一个 DHT 段（长度含 2 字节长度字段与 1 字节 TcTh）。
fn write_dht(w: &mut Writer, class_id: u8, bits: &[u8; 17], vals: &[u8]) {
    w.marker(0xC4);
    w.u16((2 + 1 + 16 + vals.len()) as u16);
    w.byte(class_id); // 高 4 位 0=DC / 1=AC，低 4 位表号
    for i in 1..=16 {
        w.byte(bits[i]);
    }
    for &v in vals {
        w.byte(v);
    }
}

// ==================== 测试 ====================

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;
    use std::vec::Vec;

    /// 遍历标记段（跳过载荷），检查头/尾/尺寸/填充规则。
    fn structural_check(jpg: &[u8], w: usize, h: usize) {
        assert!(jpg.len() > 100, "JPEG 过短");
        assert_eq!(&jpg[0..2], &[0xFF, 0xD8], "缺少 SOI");
        assert_eq!(&jpg[jpg.len() - 2..], &[0xFF, 0xD9], "缺少 EOI");

        let mut pos = 2usize;
        let mut saw_sof0 = false;
        let mut saw_sos = false;
        let mut entropy_start = 0usize;
        while pos + 4 <= jpg.len() {
            assert_eq!(jpg[pos], 0xFF, "标记对齐错误 @{pos}");
            let marker = jpg[pos + 1];
            if marker == 0xD8 {
                pos += 2;
                continue;
            }
            if marker == 0xD9 {
                break;
            }
            let len = ((jpg[pos + 2] as usize) << 8) | jpg[pos + 3] as usize;
            assert!(len >= 2, "段长度异常 @{pos}");
            if marker == 0xE0 {
                assert_eq!(&jpg[pos + 4..pos + 9], b"JFIF\0", "APP0 缺 JFIF");
            }
            if marker == 0xC0 {
                saw_sof0 = true;
                let hh = ((jpg[pos + 5] as usize) << 8) | jpg[pos + 6] as usize;
                let ww = ((jpg[pos + 7] as usize) << 8) | jpg[pos + 8] as usize;
                assert_eq!(hh, h, "SOF0 高度不符");
                assert_eq!(ww, w, "SOF0 宽度不符");
            }
            if marker == 0xDA {
                saw_sos = true;
                entropy_start = pos + 2 + len;
                break;
            }
            pos += 2 + len;
        }
        assert!(saw_sof0, "缺少 SOF0");
        assert!(saw_sos, "缺少 SOS");
        assert!(entropy_start > 0);

        // 熵数据检查：0xFF 后必须跟 0x00（字节填充），直至 EOI
        let mut i = entropy_start;
        let mut found_eoi = false;
        while i + 1 < jpg.len() {
            if jpg[i] == 0xFF {
                if jpg[i + 1] == 0xD9 {
                    found_eoi = true;
                    break;
                }
                assert_eq!(jpg[i + 1], 0x00, "熵数据 0xFF 未填充 @{i}");
                i += 2;
            } else {
                i += 1;
            }
        }
        assert!(found_eoi, "熵数据中未找到 EOI");
        // EOI 之后不应再有数据
        let eoi_at = i;
        assert!(jpg.len() - eoi_at <= 2, "EOI 后存在多余数据");
    }

    fn solid_rgb(w: usize, h: usize, r: u8, g: u8, b: u8) -> Vec<u8> {
        let mut v = Vec::with_capacity(w * h * 3);
        for _ in 0..w * h {
            v.push(r);
            v.push(g);
            v.push(b);
        }
        v
    }

    #[test]
    fn encode_solid_small() {
        let w = 16;
        let h = 16;
        let rgb = solid_rgb(w, h, 200, 100, 50);
        let mut out = vec![0u8; 65536];
        let n = jpeg_encode_rgb888(&mut out, w, h, &rgb, 90).expect("编码失败");
        structural_check(&out[..n], w, h);
    }

    #[test]
    fn encode_gradient_640x480() {
        let w = 640;
        let h = 480;
        let mut rgb = Vec::with_capacity(w * h * 3);
        for y in 0..h {
            for x in 0..w {
                rgb.push((x * 255 / w) as u8);
                rgb.push((y * 255 / h) as u8);
                rgb.push(((x + y) * 255 / (w + h)) as u8);
            }
        }
        let mut out = vec![0u8; 4 * 1024 * 1024];
        let n = jpeg_encode_rgb888(&mut out, w, h, &rgb, 85).expect("编码失败");
        structural_check(&out[..n], w, h);
        // 渐变图有平滑亮度变化，压缩后应显著大于纯色图（纯色 16x16 约 630B）
        assert!(n > 4_000, "渐变图输出过小: {n}");
    }

    #[test]
    fn encode_odd_dimensions() {
        let w = 33;
        let h = 17;
        let mut rgb = Vec::with_capacity(w * h * 3);
        for i in 0..w * h {
            rgb.push((i % 251) as u8);
            rgb.push((i % 199) as u8);
            rgb.push((i % 137) as u8);
        }
        let mut out = vec![0u8; 1 << 20];
        let n = jpeg_encode_rgb888(&mut out, w, h, &rgb, 70).expect("编码失败");
        structural_check(&out[..n], w, h);
    }

    #[test]
    fn deterministic_output() {
        let w = 64;
        let h = 64;
        let mut rgb = Vec::with_capacity(w * h * 3);
        for i in 0..w * h {
            rgb.push((i * 7 % 256) as u8);
            rgb.push((i * 11 % 256) as u8);
            rgb.push((i * 13 % 256) as u8);
        }
        let mut a = vec![0u8; 1 << 20];
        let mut b = vec![0u8; 1 << 20];
        let na = jpeg_encode_rgb888(&mut a, w, h, &rgb, 60).unwrap();
        let nb = jpeg_encode_rgb888(&mut b, w, h, &rgb, 60).unwrap();
        assert_eq!(na, nb, "两次编码结果不一致");
        assert_eq!(&a[..na], &b[..nb], "两次编码字节不一致");
    }

    #[test]
    fn buffer_too_small() {
        let w = 64;
        let h = 64;
        let rgb = solid_rgb(w, h, 128, 128, 128);
        let mut out = vec![0u8; 64];
        match jpeg_encode_rgb888(&mut out, w, h, &rgb, 95) {
            Err(JpegError::BufferTooSmall { needed }) => {
                assert!(needed > 64, "needed 估计过小");
                let mut big = vec![0u8; needed + 4096];
                let n = jpeg_encode_rgb888(&mut big, w, h, &rgb, 95).unwrap();
                structural_check(&big[..n], w, h);
            }
            other => panic!("期望 BufferTooSmall，得到 {:?}", other),
        }
    }

    #[test]
    fn bad_arguments() {
        let mut out = vec![0u8; 1024];
        assert_eq!(
            jpeg_encode_rgb888(&mut out, 0, 10, &[], 50),
            Err(JpegError::BadDimensions)
        );
        assert_eq!(
            jpeg_encode_rgb888(&mut out, 10, 10, &[0u8; 10], 50),
            Err(JpegError::BadDimensions)
        );
        assert_eq!(
            jpeg_encode_rgb888(&mut out, 10, 10, &[0u8; 300], 0),
            Err(JpegError::BadQuality)
        );
        assert_eq!(
            jpeg_encode_rgb888(&mut out, 10, 10, &[0u8; 300], 101),
            Err(JpegError::BadQuality)
        );
    }

    /// 亮暗对比：纯黑图应显著小于随机噪声图（验证熵编码生效）。
    #[test]
    fn noise_compresses_worse_than_flat() {
        let w = 64;
        let h = 64;
        let flat = solid_rgb(w, h, 0, 0, 0);
        let mut noise = Vec::with_capacity(w * h * 3);
        let mut seed = 0x12345678u32;
        for _ in 0..w * h * 3 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            noise.push((seed >> 24) as u8);
        }
        let mut out_a = vec![0u8; 1 << 20];
        let mut out_b = vec![0u8; 1 << 20];
        let na = jpeg_encode_rgb888(&mut out_a, w, h, &flat, 90).unwrap();
        let nb = jpeg_encode_rgb888(&mut out_b, w, h, &noise, 90).unwrap();
        assert!(na < nb, "平坦图应比噪声图小: {na} vs {nb}");
    }
}
