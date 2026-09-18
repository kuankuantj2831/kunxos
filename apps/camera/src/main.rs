// KunxOS 相机应用（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由 launcher 经 fork + execve 拉起（/bin/camera）。职责：
//   1. 连接相机服务 camerad（/var/run/camerad.ctl 命令通道 + resp 响应 +
//      /tmp/cam 预览帧文件），拉取真实测试帧（QEMU SimCamera / Pi4 硬件）
//   2. 取景器：YUV420 → RGB 转换 + 最近邻缩放绘制（540x405 区域）
//   3. 快门：PHOTO → camerad 单帧采集 + JPEG 编码 → /data/photos/photo_XXXXXX.jpg，
//      取景器右下显示已保存文件名，缩略图取最近一帧预览
//   4. 录像：REC 按钮切换 REC_START / REC_STOP（camerad 写 /data/videos/rec_XXXXXX.kxv）
//   5. camerad 不可用时回退为模拟取景器（原行为），保证应用始终可用
//
// 协议详见 kunxserv/camerad/src/main.rs 头注释。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::global_asm;
use core::panic::PanicInfo;

#[path = "../../../shared/cjk_font_camera.rs"]
mod cjk_font;

#[path = "../../../shared/app_kit.rs"]
mod kit;

use kit::*;

// ==================== camerad 协议常量 ====================

const CAM_CTL: &str = "/var/run/camerad.ctl";
const CAM_RESP: &str = "/var/run/camerad.resp";
const CAM_LATEST: &str = "/tmp/cam/latest";
const FRAME_DIR: &str = "/tmp/cam";

/// 预览配置（与 camerad 默认一致）。
const PW: usize = 320;
const PH: usize = 240;
const FRAME_RING: usize = 6;
/// 帧文件头长度（camerad 侧 22B：magic+seq+ts+w+h+fmt+pad）。
const FRAME_HDR: usize = 22;
const FRAME_PAYLOAD: usize = PW * PH * 3 / 2;

const SYS_READ: usize = 63;
const SYS_FTRUNCATE: usize = 46;
const SYS_LSEEK: usize = 62;

// ==================== 布局与配色 ====================

const C_BG: u32 = 0x00_F7_F8_FA;
/// 三分线/取景框线：绘制在黑色取景器画面（内容）之上，保持中灰可见。
const C_GRID: u32 = 0x00_4A_5A_6A;
/// 快门白色圆环（绿色强调玻璃上的彩色表面，保留白环）。
const C_SHUTTER: u32 = 0x00_FF_FF_FF;
/// 快门内芯：深色光圈（绿色玻璃上的对比件，非主题面，保留深色）。
const C_SHUTTER_IN: u32 = 0x00_10_18_28;
const C_REC: u32 = 0x00_E7_4C_3C;
const C_TEXT: u32 = 0x00_1A_1C_22;
const C_DIM: u32 = 0x00_6B_72_80;
const C_SAVE: u32 = 0x00_00_B9_6B;

const SHUTTER_CX: i32 = (DISPLAY_W / 2) as i32;
const SHUTTER_CY: i32 = 850;
const SHUTTER_R: i32 = 44;
const THUMB_X: i32 = 24;
const THUMB_Y: i32 = 780;
const REC_BTN_X: i32 = 430;
const REC_BTN_Y: i32 = 775;
const REC_BTN_W: i32 = 86;
const REC_BTN_H: i32 = 46;

/// 取景器绘制区域（540x405，4:3，垂直居中于 100..700）。
const VIEW_X: i32 = 0;
const VIEW_Y: i32 = 197;
const VIEW_W: i32 = 540;
const VIEW_H: i32 = 405;

// ==================== 状态 ====================

/// camerad 是否可用（探测失败回退模拟）。
static mut CAM_OK: bool = false;
/// 已启动预览流。
static mut PREVIEW_ON: bool = false;
/// 最近一次读取的帧序号。
static mut LAST_SEQ: u32 = 0;
/// 拍照计数（缩略图展示用）。
static mut PHOTO_COUNT: u32 = 0;
/// 最近保存的照片信息显示计时（>0 时显示文件名）。
static mut SAVED_SHOW: u32 = 0;
static mut SAVED_NAME: [u8; 24] = [0u8; 24];
static mut SAVED_LEN: usize = 0;
/// 拍照处理进度（0..100）。
static mut PROCESSING: u32 = 0;
/// 闪光帧计数。
static mut FLASH: u32 = 0;
/// 录像状态与计时。
static mut RECORDING: bool = false;
static mut REC_TICKS: u32 = 0;
/// 请求 id 计数器。
static mut REQ_ID: u32 = 1;
/// 缩略图（RGB888 160x120，取自最近预览帧）。
static mut THUMB: [u8; 160 * 120 * 3] = [0u8; 160 * 120 * 3];
static mut THUMB_VALID: bool = false;

// ==================== 文件/协议 I/O ====================

static mut YUV_BUF: [u8; FRAME_PAYLOAD + 64] = [0u8; FRAME_PAYLOAD + 64];
static mut RESP_BUF: [u8; 256] = [0u8; 256];
static mut CMD_BUF: [u8; 96] = [0u8; 96];
static mut LATEST_BUF: [u8; 16] = [0u8; 16];

#[inline(always)]
unsafe fn syscall3(n: usize, a1: usize, a2: usize, a3: usize) -> usize {
    let ret: usize;
    core::arch::asm!(
        "svc #0",
        in("x8") n,
        inlateout("x0") a1 => ret,
        in("x1") a2,
        in("x2") a3,
        options(nostack)
    );
    ret
}

unsafe fn read_fd(fd: usize, buf: &mut [u8]) -> isize {
    syscall3(SYS_READ, fd, buf.as_mut_ptr() as usize, buf.len()) as isize
}

unsafe fn close_fd(fd: usize) {
    syscall3(SYS_CLOSE, fd, 0, 0);
}

/// 探测 camerad：命令文件可打开即可用。
unsafe fn cam_probe() -> bool {
    let fd = open(CAM_CTL, O_WRONLY);
    if fd >= 0 {
        close_fd(fd as usize);
        true
    } else {
        false
    }
}

/// 发送一条命令 `#<id> <cmd>` 到 ctl 文件。
unsafe fn cam_send(id: u32, cmd: &[u8]) -> bool {
    let fd = open(CAM_CTL, O_WRONLY | 0x40 /*O_CREAT*/);
    if fd < 0 {
        return false;
    }
    let buf = &mut *core::ptr::addr_of_mut!(CMD_BUF);
    let mut n = 0;
    buf[n] = b'#';
    n += 1;
    let mut v = id;
    // u32 十进制至多 10 位：数组按 10 位开（此前 [0u8;4]，REQ_ID 累计到
    // 10000 后 digits[4] 越界 panic）。
    let mut digits = [0u8; 10];
    let mut d = 0;
    loop {
        digits[d] = b'0' + (v % 10) as u8;
        d += 1;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    while d > 0 {
        d -= 1;
        buf[n] = digits[d];
        n += 1;
    }
    buf[n] = b' ';
    n += 1;
    for &b in cmd {
        if n >= buf.len() - 2 {
            break;
        }
        buf[n] = b;
        n += 1;
    }
    buf[n] = b'\n';
    n += 1;
    write_fd(fd as usize, &buf[..n]);
    close_fd(fd as usize);
    true
}

/// 读取 resp 文件中 `#<id> ` 行的响应体到 RESP_BUF，返回长度；无则返回 0。
/// 读取后清空 resp 文件（应用侧负责清理）。
unsafe fn cam_poll_resp(id: u32) -> usize {
    let fd = open(CAM_RESP, O_RDONLY);
    if fd < 0 {
        return 0;
    }
    syscall3(SYS_LSEEK, fd as usize, 0, 0);
    let n = read_fd(fd as usize, &mut *core::ptr::addr_of_mut!(RESP_BUF));
    close_fd(fd as usize);
    // 清空
    let fd2 = open(CAM_RESP, O_WRONLY);
    if fd2 >= 0 {
        syscall3(SYS_FTRUNCATE, fd2 as usize, 0, 0);
        close_fd(fd2 as usize);
    }
    if n <= 0 {
        return 0;
    }
    let data = &RESP_BUF[..n as usize];
    // 找 "#<id> "（'#' + 至多 10 位数字 + 空格 = 12）
    let mut prefix = [0u8; 12];
    let mut pn = 0;
    prefix[pn] = b'#';
    pn += 1;
    let mut v = id;
    // 与 cam_send 同款：10 位十进制（[0u8;4] 在 REQ_ID ≥ 10000 时越界）
    let mut digits = [0u8; 10];
    let mut d = 0;
    loop {
        digits[d] = b'0' + (v % 10) as u8;
        d += 1;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    while d > 0 {
        d -= 1;
        prefix[pn] = digits[d];
        pn += 1;
    }
    prefix[pn] = b' ';
    pn += 1;
    // 逐行匹配
    let mut start = 0usize;
    for i in 0..=data.len() {
        if i == data.len() || data[i] == b'\n' {
            let line = &data[start..i];
            if line.len() >= pn && &line[..pn] == &prefix[..pn] {
                let boff = start + pn;
                let blen = (i - boff).min(RESP_BUF.len());
                // body 是 RESP_BUF 的子切片：copy_from_slice 要求不重叠（UB），
                // 用 copy_within 原地前移。
                RESP_BUF.copy_within(boff..boff + blen, 0);
                return blen;
            }
            start = i + 1;
        }
    }
    0
}

/// 发送命令并等待响应（最多 timeout_ms），返回响应体长度。
unsafe fn cam_cmd(id: u32, cmd: &[u8], timeout_ms: usize) -> usize {
    if !cam_send(id, cmd) {
        return 0;
    }
    let mut waited = 0usize;
    while waited < timeout_ms {
        let n = cam_poll_resp(id);
        if n > 0 {
            return n;
        }
        sleep_ms(20);
        waited += 20;
    }
    0
}

/// 读取最新帧序号（/tmp/cam/latest）。
unsafe fn read_latest_seq() -> Option<u32> {
    let fd = open(CAM_LATEST, O_RDONLY);
    if fd < 0 {
        return None;
    }
    let n = read_fd(fd as usize, &mut *core::ptr::addr_of_mut!(LATEST_BUF));
    close_fd(fd as usize);
    if n <= 0 {
        return None;
    }
    let mut seq: u32 = 0;
    for &b in &LATEST_BUF[..n as usize] {
        if !b.is_ascii_digit() {
            break;
        }
        seq = seq * 10 + (b - b'0') as u32;
    }
    Some(seq)
}

/// 读取指定序号的预览帧（环形文件），校验头部后载入 YUV_BUF。
unsafe fn read_preview_frame(seq: u32) -> bool {
    let idx = (seq as usize) % FRAME_RING;
    let mut path = [0u8; 40];
    let mut n = 0;
    for &b in FRAME_DIR.as_bytes() {
        path[n] = b;
        n += 1;
    }
    path[n] = b'/';
    n += 1;
    path[n..n + 6].copy_from_slice(b"frame_");
    n += 6;
    path[n] = b'0' + idx as u8;
    n += 1;
    path[n..n + 4].copy_from_slice(b".yuv");
    n += 4;
    path[n] = 0;
    // 注意 n 此时是完整路径长度（NUL 不计入）—— 此前 &path[..n-1] 会把
    // 结尾 'v' 切掉，open 的永远是 /tmp/cam/frame_X.yu，取景器恒黑。
    let fd = open(core::str::from_utf8(&path[..n]).unwrap_or(""), O_RDONLY);
    if fd < 0 {
        return false;
    }
    let buf = &mut *core::ptr::addr_of_mut!(YUV_BUF);
    let got = read_fd(fd as usize, &mut buf[..FRAME_HDR + FRAME_PAYLOAD]);
    close_fd(fd as usize);
    if got < (FRAME_HDR + FRAME_PAYLOAD) as isize {
        return false;
    }
    // 校验头部 seq 一致（防止读到半写帧）
    let hdr_seq = u32::from_le_bytes(buf[4..8].try_into().unwrap());
    hdr_seq == seq
}

// ==================== 渲染 ====================

fn clamp8(v: i32) -> u8 {
    if v < 0 {
        0
    } else if v > 255 {
        255
    } else {
        v as u8
    }
}

/// BT.601 YUV420 → RGB888（整数定点）。
fn yuv_to_rgb(y: i32, u: i32, v: i32) -> u32 {
    let c = y - 16;
    let d = u - 128;
    let e = v - 128;
    let r = (298 * c + 409 * e + 128) >> 8;
    let g = (298 * c - 100 * d - 208 * e + 128) >> 8;
    let b = (298 * c + 516 * d + 128) >> 8;
    (clamp8(r) as u32) << 16 | (clamp8(g) as u32) << 8 | clamp8(b) as u32
}

/// 绘制一帧预览：YUV_BUF → RGB → 最近邻缩放 → fb。
unsafe fn draw_preview_frame() {
    let yuv = &*core::ptr::addr_of!(YUV_BUF);
    let y_plane = &yuv[..PW * PH];
    let u_plane = &yuv[PW * PH..PW * PH + (PW / 2) * (PH / 2)];
    let v_plane = &yuv[PW * PH + (PW / 2) * (PH / 2)..];

    // 最近邻缩放：目标 (tx,ty) ← 源 (tx*PW/VIEW_W, ty*PH/VIEW_H)
    for ty in 0..VIEW_H as usize {
        let sy = ty * PH / VIEW_H as usize;
        for tx in 0..VIEW_W as usize {
            let sx = tx * PW / VIEW_W as usize;
            let y = y_plane[sy * PW + sx] as i32;
            let u = u_plane[(sy / 2) * (PW / 2) + sx / 2] as i32;
            let v = v_plane[(sy / 2) * (PW / 2) + sx / 2] as i32;
            put_pixel(VIEW_X + tx as i32, VIEW_Y + ty as i32, yuv_to_rgb(y, u, v));
        }
    }
    // 更新缩略图（源 2x2 → 缩略图 1px，160x120）
    for ty in 0..120usize {
        for tx in 0..160usize {
            let sx = tx * 2;
            let sy = ty * 2;
            let y = y_plane[sy * PW + sx] as i32;
            let u = u_plane[(sy / 2) * (PW / 2) + sx / 2] as i32;
            let v = v_plane[(sy / 2) * (PW / 2) + sx / 2] as i32;
            let rgb = yuv_to_rgb(y, u, v);
            let i = (ty * 160 + tx) * 3;
            THUMB[i] = ((rgb >> 16) & 0xFF) as u8;
            THUMB[i + 1] = ((rgb >> 8) & 0xFF) as u8;
            THUMB[i + 2] = (rgb & 0xFF) as u8;
        }
    }
    THUMB_VALID = true;
}

/// 从缩略图缓冲绘制缩略图卡片。
fn draw_thumb_card(x: i32, y: i32) {
    if !unsafe { THUMB_VALID } {
        return;
    }
    for ty in 0..56usize {
        for tx in 0..110usize {
            let sx = tx * 160 / 110;
            let sy = ty * 120 / 56;
            let i = (sy * 160 + sx) * 3;
            let (r, g, b) = unsafe {
                (
                    THUMB[i] as u32,
                    THUMB[i + 1] as u32,
                    THUMB[i + 2] as u32,
                )
            };
            put_pixel(x + tx as i32, y + ty as i32, (r << 16) | (g << 8) | b);
        }
    }
    draw_glass(x, y, 110, 56, 14);
}

/// 模拟取景器（camerad 不可用时回退）。取景器画面区域保持深色（是内容不是主题）。
fn draw_sim_viewfinder() {
    let mut seed: u32 = 0x12345678;
    for y in 100..700 {
        for x in 0..DISPLAY_W as i32 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let v = ((seed >> 16) & 0xFF) as u32;
            let color = 0x00_10_14_20 | (v << 8) | (v << 16);
            put_pixel(x, y, color);
        }
    }
    draw_line(0, 100, DISPLAY_W as i32, 100, C_GRID);
    draw_line(0, 700, DISPLAY_W as i32, 700, C_GRID);
    draw_line(DISPLAY_W as i32 / 3, 100, DISPLAY_W as i32 / 3, 700, C_GRID);
    draw_line(DISPLAY_W as i32 / 3 * 2, 100, DISPLAY_W as i32 / 3 * 2, 700, C_GRID);
}

fn render() {
    draw_gradient_bg(C_BG, glass_dark());
    draw_title_bar("相机".as_bytes());

    // 取景器
    if unsafe { CAM_OK } {
        // 已由 draw_preview_frame 绘制（仅在帧更新时调用）
        draw_line(0, VIEW_Y, DISPLAY_W as i32, VIEW_Y, C_GRID);
        draw_line(0, VIEW_Y + VIEW_H, DISPLAY_W as i32, VIEW_Y + VIEW_H, C_GRID);
        draw_line(DISPLAY_W as i32 / 3, VIEW_Y, DISPLAY_W as i32 / 3, VIEW_Y + VIEW_H, C_GRID);
        draw_line(DISPLAY_W as i32 / 3 * 2, VIEW_Y, DISPLAY_W as i32 / 3 * 2, VIEW_Y + VIEW_H, C_GRID);
    } else {
        draw_sim_viewfinder();
    }

    // REC 指示 + 计时
    if unsafe { RECORDING } {
        draw_disc(36, 80, 6, C_REC);
        draw_text(52, 74, "REC".as_bytes(), C_REC);
        let secs = unsafe { REC_TICKS } * 16 / 1000;
        let mut tbuf = [0u8; 8];
        // 分钟两位数钳制（>99 分钟时 b'0'+x 会溢出成非数字字节）。
        let mm = (secs / 60).min(99);
        tbuf[0] = b'0' + (mm / 10) as u8;
        tbuf[1] = b'0' + (mm % 10) as u8;
        tbuf[2] = b':';
        tbuf[3] = b'0' + ((secs % 60) / 10) as u8;
        tbuf[4] = b'0' + ((secs % 60) % 10) as u8;
        draw_text(96, 74, &tbuf[..5], C_REC);
    }

    // 快门
    draw_glass_style(
        SHUTTER_CX - SHUTTER_R,
        SHUTTER_CY - SHUTTER_R,
        SHUTTER_R * 2,
        SHUTTER_R * 2,
        SHUTTER_R,
        &STYLE_ACCENT,
    );
    draw_disc(SHUTTER_CX, SHUTTER_CY, SHUTTER_R - 10, C_SHUTTER_IN);
    draw_circle(SHUTTER_CX, SHUTTER_CY, SHUTTER_R - 10, C_SHUTTER);

    // 录像按钮（液态玻璃小按钮）：录像中红色内芯用白字，待机白色内芯用深字。
    let rec_color = if unsafe { RECORDING } { C_REC } else { C_BTN };
    draw_glass_style(REC_BTN_X, REC_BTN_Y, REC_BTN_W, REC_BTN_H, 12, &STYLE_ACCENT);
    fill_rect(REC_BTN_X + 4, REC_BTN_Y + 4, REC_BTN_W - 8, REC_BTN_H - 8, rec_color);
    let rec_label_col = if unsafe { RECORDING } { 0x00_FF_FF_FF } else { C_TEXT };
    draw_text(REC_BTN_X + 18, REC_BTN_Y + 14, if unsafe { RECORDING } { "停止".as_bytes() } else { "录像".as_bytes() }, rec_label_col);

    // 缩略图（最近照片）
    draw_thumb_card(THUMB_X, THUMB_Y);

    // 保存提示
    let show = unsafe { SAVED_SHOW };
    if show > 0 {
        draw_glass(150, 210, 240, 40, 10);
        draw_center(222, "已保存 ".as_bytes(), C_SAVE);
        let name = unsafe { &SAVED_NAME[..SAVED_LEN] };
        draw_center(240, name, C_TEXT);
    }

    // 处理中进度
    let p = unsafe { PROCESSING };
    if p > 0 && p < 100 {
        draw_glass(60, 200, DISPLAY_W as i32 - 120, 8, 2);
        fill_rect(60, 200, (DISPLAY_W as i32 - 120) * (p as i32) / 100, 8, C_REC);
        draw_center(220, "处理中...".as_bytes(), C_DIM);
    }

    draw_text(16, DISPLAY_H as i32 - 40, "点击快门拍照 · REC 录像".as_bytes(), C_DIM);
    draw_sys_nav();
}

// ==================== 相机控制 ====================

/// 启动 camerad 预览：CONFIG → PREVIEW_START。
unsafe fn camera_start() -> bool {
    let id = &mut *core::ptr::addr_of_mut!(REQ_ID);
    // CONFIG 320x240@15 yuv420
    let cfg = b"CONFIG 0 320 240 15 yuv420";
    let r1 = cam_cmd(*id, cfg, 1500);
    *id += 1;
    if r1 == 0 {
        return false;
    }
    let r2 = cam_cmd(*id, b"PREVIEW_START 0", 1500);
    *id += 1;
    if r2 == 0 {
        return false;
    }
    PREVIEW_ON = true;
    true
}

/// 停止预览。
unsafe fn camera_stop() {
    if !PREVIEW_ON {
        return;
    }
    let id = &mut *core::ptr::addr_of_mut!(REQ_ID);
    cam_cmd(*id, b"PREVIEW_STOP 0", 500);
    *id += 1;
    PREVIEW_ON = false;
}

/// 拍照：PHOTO → 等待 JPEG 落盘 → 更新保存提示。
unsafe fn take_photo() {
    let id = &mut *core::ptr::addr_of_mut!(REQ_ID);
    let n = cam_cmd(*id, b"PHOTO 0", 10_000);
    *id += 1;
    if n == 0 {
        print("[camera] PHOTO 超时\n");
        return;
    }
    // 响应体：`<num> <size> <seq>`；取第一个 token 为照片序号
    let body = &RESP_BUF[..n];
    let mut num = 0u32;
    let mut started = false;
    for &b in body {
        if b.is_ascii_digit() {
            num = num * 10 + (b - b'0') as u32;
            started = true;
        } else if started {
            break;
        }
    }
    if num == 0 {
        print("[camera] PHOTO 失败\n");
        return;
    }
    // 保存名：photo_NNNNNN.jpg
    let name = &mut *core::ptr::addr_of_mut!(SAVED_NAME);
    let mut n2 = 0;
    for &b in b"photo_" {
        name[n2] = b;
        n2 += 1;
    }
    let mut digs = [0u8; 6];
    let mut v = num;
    for i in (0..6).rev() {
        digs[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    for &b in &digs {
        name[n2] = b;
        n2 += 1;
    }
    for &b in b".jpg" {
        name[n2] = b;
        n2 += 1;
    }
    SAVED_LEN = n2;
    SAVED_SHOW = 150; // ~2.4s
    PHOTO_COUNT += 1;
    print("[camera] 照片已保存 ");
    print(core::str::from_utf8(&name[..n2]).unwrap_or(""));
    print("\n");
}

/// 切换录像状态。
unsafe fn toggle_rec() {
    let id = &mut *core::ptr::addr_of_mut!(REQ_ID);
    if RECORDING {
        cam_cmd(*id, b"REC_STOP 0", 1000);
        *id += 1;
        RECORDING = false;
        REC_TICKS = 0;
        print("[camera] 录像已停止\n");
    } else {
        let r = cam_cmd(*id, b"REC_START 0", 1000);
        *id += 1;
        if r > 0 {
            RECORDING = true;
            REC_TICKS = 0;
            print("[camera] 录像已开始\n");
        }
    }
}

// ==================== 主循环 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[camera] 正在启动\n");
    if !fb_init() {
        print("[camera] /dev/fb0 mmap 失败，无帧缓冲\n");
        unsafe { exit(1); }
    }
    print("[camera] 就绪\n");

    unsafe {
        if cam_probe() {
            print("[camera] 检测到 camerad，正在连接\n");
            if camera_start() {
                print("[camera] 预览已启动\n");
                CAM_OK = true;
            } else {
                print("[camera] 预览启动失败，模拟模式\n");
            }
        } else {
            print("[camera] 无 camerad，模拟模式\n");
        }
    }

    let mut dirty = true;
    let mut last_draw_seq: u32 = u32::MAX;
    loop {
        let ev = poll_input();
        if ev.moved {
            cursor_overlay_move();
        }
        if ev.nav != NavAction::None {
            print("[nav] 系统导航 -> 退出\n");
            // 手势/三键导航走 exit() 前必须先停预览流：否则 camerad 侧
            // 环形帧文件被持续覆写（与 break 路径的 camera_stop 对齐）。
            unsafe {
                if CAM_OK {
                    camera_stop();
                }
            }
            unsafe { exit(nav_exit_code(ev.nav)); }
        }

        if let Some((x, y)) = ev.tap {
            if back_hit(x, y) {
                print("[camera] 返回 -> 退出\n");
                break;
            }
            let dx = x - SHUTTER_CX;
            let dy = y - SHUTTER_CY;
            if dx * dx + dy * dy <= SHUTTER_R * SHUTTER_R && unsafe { PROCESSING == 0 } {
                print("[camera] 快门！\n");
                unsafe {
                    FLASH = 4;
                    PROCESSING = 1;
                }
                dirty = true;
            }
            // 录像按钮
            if x >= REC_BTN_X && x <= REC_BTN_X + REC_BTN_W && y >= REC_BTN_Y && y <= REC_BTN_Y + REC_BTN_H
            {
                unsafe { toggle_rec(); }
                dirty = true;
            }
        }
        match ev.key {
            KeyAction::Esc => {
                print("[camera] ESC -> 退出\n");
                break;
            }
            _ => {}
        }
        if poll_tty_esc() {
            print("[camera] ESC -> 退出\n");
            break;
        }

        unsafe {
            // 拍照流程：进度动画 + 请求 PHOTO
            if PROCESSING == 1 {
                // 首次触发拍照请求
                PROCESSING = 2;
                take_photo();
            } else if PROCESSING > 0 && PROCESSING < 100 {
                PROCESSING += 6;
                if PROCESSING >= 100 {
                    PROCESSING = 0;
                }
            }

            // 预览帧拉取
            if CAM_OK && PREVIEW_ON {
                if let Some(seq) = read_latest_seq() {
                    if seq != last_draw_seq {
                        if read_preview_frame(seq) {
                            last_draw_seq = seq;
                            draw_preview_frame();
                            dirty = true;
                        }
                    }
                }
            }

            // 录像计时
            if RECORDING {
                REC_TICKS += 1;
                dirty = true;
            }
            // 保存提示计时
            if SAVED_SHOW > 0 {
                SAVED_SHOW -= 1;
                if SAVED_SHOW == 0 {
                    dirty = true;
                }
            }
            if FLASH > 0 {
                FLASH -= 1;
                dirty = true;
            }
        }

        if dirty {
            render();
            // 闪光帧覆盖全白
            if unsafe { FLASH > 0 } {
                fill_rect(0, 0, DISPLAY_W as i32, DISPLAY_H as i32, 0x00_FF_FF_FF);
            }
            flip();
            cursor_overlay_redraw();
            dirty = false;
            kit::font_init_checked();
            if kit::font_just_refreshed() { dirty = true; }
        }
        sleep_ms(16);
    }

    unsafe {
        if CAM_OK {
            camera_stop();
        }
    }
    print("[camera] 正在退出\n");
    unsafe { exit(0); }
}

// ==================== 入口 ====================

global_asm!(
    ".section .text._start, \"ax\"",
    ".global _start",
    ".type _start, %function",
    "_start:",
    "    mov x2, sp",
    "    ldr x0, [x2], #8",
    "    mov x1, x2",
    "    bl k_main",
    "    mov x8, #93",
    "    svc #0",
    "    b .",
);

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // 诊断：把 panic 消息与位置（文件:行:列）写入 fd 2（无堆，固定缓冲）。
    struct BufWriter {
        buf: [u8; 256],
        len: usize,
    }
    impl core::fmt::Write for BufWriter {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let b = s.as_bytes();
            let k = b.len().min(self.buf.len() - self.len);
            self.buf[self.len..self.len + k].copy_from_slice(&b[..k]);
            self.len += k;
            Ok(())
        }
    }
    let mut w = BufWriter { buf: [0u8; 256], len: 0 };
    let _ = core::fmt::Write::write_fmt(&mut w, core::format_args!("{}\n", info));
    unsafe {
        write_fd(2, b"PANIC: camera\n");
        write_fd(2, &w.buf[..w.len]);
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
