// KunxOS 地图应用（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由 launcher 经 fork + execve 拉起（/bin/map）。职责：
//   1. 打开 /dev/fb0（openat）并 mmap 到用户空间，直接写入 RGBA8888 像素
//   2. 静态地图：街区方块 + 道路网格 + 河流 + 定位标记
//   3. 左上角罗盘 N（按 sensord 航向旋转指针）、右上角「+ / -」缩放按钮
//   4. 底部传感器面板：航向 / 姿态 / 步数 / 抬腕 / 屏幕方向（自动旋转演示）
//     与 GPS 定位（positiond 状态文件；WGS84 坐标文本 + 新鲜度）
//   5. 点击左上角「返回」或按 ESC 退出（exit(0)），由 launcher wait4 回收

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::global_asm;
use core::panic::PanicInfo;

#[path = "../../../shared/cjk_font_map.rs"]
mod cjk_font;

#[path = "../../../shared/app_kit.rs"]
mod kit;

#[path = "../../../shared/sensor_client.rs"]
mod sensor_client;

use kit::*;
use sensor_client::{orientation_name, position_state, sensor_state};

// ==================== 布局与配色 ====================

const C_BG: u32 = 0x00_F7_F8_FA;
/// 街区方块：浅灰（浅色地图底）。
const C_BLOCK: u32 = 0x00_E2_E7_EF;
/// 道路：白色路网（压在浅灰街区之上）。
const C_ROAD: u32 = 0x00_FF_FF_FF;
/// 河流：浅蓝。
const C_RIVER: u32 = 0x00_A5_CE_E8;
/// 公园/绿地：浅绿。
const C_PARK: u32 = 0x00_C2_E5_C3;
const C_MARKER: u32 = 0x00_E7_4C_3C;
const C_TEXT: u32 = 0x00_1A_1C_22;
const C_DIM: u32 = 0x00_6B_72_80;
const C_BTN: u32 = 0x00_FF_FF_FF;
/// 罗盘刻度环：中灰描边（浅色地图上保持可读）。
const C_BTN_BORDER: u32 = 0x00_B9_C2_CE;
const C_ACCENT: u32 = 0x00_00_B9_6B;
const C_WARN: u32 = 0x00_E2_96_06;

const MAP_TOP: i32 = 52;
const MAP_BOTTOM: i32 = 760;

/// 罗盘中心（左上角）。
const COMP_X: i32 = 40;
const COMP_Y: i32 = 150;
const COMP_R: i32 = 22;

// ==================== 状态 ====================

/// 缩放级别（网格步长像素）。
static mut ZOOM: i32 = 60;

/// 传感器面板数据缓存（每 300ms 刷新一次）。
static mut SENSOR_VALID: bool = false;
static mut SENSOR_HEADING: i32 = 0;
static mut SENSOR_STEPS: u32 = 0;
static mut SENSOR_RAISES: u32 = 0;
static mut SENSOR_ORIENT: u8 = 0;
static mut SENSOR_ROLL: i32 = 0;
static mut SENSOR_PITCH: i32 = 0;
static mut GPS_VALID: bool = false;
static mut GPS_LAT: i64 = 0;
static mut GPS_LON: i64 = 0;
static mut GPS_SATS: u32 = 0;
static mut GPS_UTC: (u32, u32, u32) = (0, 0, 0);

/// 传感器面板数据上次刷新帧计数。
static mut SENSOR_TICK: u64 = 0;

/// 每 300ms（约 19 帧 @16ms）刷新一次传感器面板。
fn refresh_sensor_panel() {
    unsafe {
        let now = now_us() / 1000;
        if SENSOR_TICK == 0 || now - SENSOR_TICK >= 300 {
            SENSOR_TICK = now;
            let s = sensor_state();
            SENSOR_VALID = s.valid;
            SENSOR_HEADING = s.heading;
            SENSOR_STEPS = s.steps;
            SENSOR_RAISES = s.raises;
            SENSOR_ORIENT = s.orientation as u8;
            SENSOR_ROLL = s.roll;
            SENSOR_PITCH = s.pitch;
            let p = position_state();
            GPS_VALID = p.valid && p.has_fix;
            GPS_LAT = p.lat;
            GPS_LON = p.lon;
            GPS_SATS = p.sats;
            GPS_UTC = p.utc;
        }
    }
}

// ==================== 三角查找表（无浮点库） ====================

/// sin(θ)·1024，θ = 0..=90（度）；其余象限由对称性推导。
static SIN_TAB: [i32; 91] = [
    0, 18, 36, 54, 71, 89, 107, 125, 143, 160, 178, 195,
    213, 230, 248, 265, 282, 299, 316, 333, 350, 367, 384, 400,
    416, 433, 449, 465, 481, 496, 512, 527, 543, 558, 573, 587,
    602, 616, 630, 644, 658, 672, 685, 698, 711, 724, 737, 749,
    761, 773, 784, 796, 807, 818, 828, 839, 849, 859, 868, 878,
    887, 896, 904, 912, 920, 928, 935, 943, 949, 956, 962, 968,
    974, 979, 984, 989, 994, 998, 1002, 1005, 1008, 1011, 1014, 1016,
    1018, 1020, 1022, 1023, 1023, 1024, 1024,
];

/// sin(θ)，θ 为任意整数度（返回值 ×1024）。
fn sin_deg(deg: i32) -> i32 {
    let d = deg.rem_euclid(360);
    match d {
        0..=90 => SIN_TAB[d as usize],
        91..=180 => SIN_TAB[(180 - d) as usize],
        181..=270 => -SIN_TAB[(d - 180) as usize],
        _ => -SIN_TAB[(360 - d) as usize],
    }
}

/// cos(θ)，θ 为任意整数度（返回值 ×1024）。
fn cos_deg(deg: i32) -> i32 {
    sin_deg(deg + 90)
}

// ==================== 渲染 ====================

/// 绘制罗盘：固定刻度环 + 按航向旋转的指针与 N 标记。
fn draw_compass(heading: i32) {
    // 刻度环。
    draw_circle(COMP_X, COMP_Y, COMP_R, C_BTN_BORDER);
    draw_circle(COMP_X, COMP_Y, COMP_R - 3, C_BTN_BORDER);
    // 指针：指向「航向反方向」表示地图上北偏移（演示：指针 = 当前航向）。
    let ang = heading.rem_euclid(360);
    // 指针（红）：从中心沿 -ang 方向。
    let px = COMP_X + (sin_deg(ang) * (COMP_R - 4) / 1024);
    let py = COMP_Y - (cos_deg(ang) * (COMP_R - 4) / 1024);
    draw_line(COMP_X, COMP_Y, px, py, C_MARKER);
    // 反方向短线（灰）：指示背向。
    let qx = COMP_X - (sin_deg(ang) * (COMP_R - 8) / 1024);
    let qy = COMP_Y + (cos_deg(ang) * (COMP_R - 8) / 1024);
    draw_line(COMP_X, COMP_Y, qx, qy, C_DIM);
    // N 标记：旋转到航向方向。
    let nx = COMP_X + (sin_deg(ang) * (COMP_R + 8) / 1024);
    let ny = COMP_Y - (cos_deg(ang) * (COMP_R + 8) / 1024);
    draw_text(nx - 4, ny - 7, "N".as_bytes(), C_TEXT);
    // 中心点。
    draw_disc(COMP_X, COMP_Y, 2, C_TEXT);
}

/// 绘制传感器面板（航向 / 姿态 / 步数 / 抬腕 / 屏幕方向 / GPS）。
fn draw_sensor_panel() {
    let x = 12;
    let mut y = MAP_BOTTOM + 8;

    // 面板底：玻璃面板。
    let w = display_w() as i32 - 24;
    let h = 132;
    draw_glass(x, y - 6, w, h, 12);

    // 航向 + 屏幕方向（自动旋转演示）。
    let mut line = [0u8; 64];
    let mut i = 0usize;
    if unsafe { SENSOR_VALID } {
        let hd = unsafe { SENSOR_HEADING };
        push_text(&mut line, &mut i, "航向 ".as_bytes());
        push_dec(&mut line, &mut i, hd as i64);
        push_text(&mut line, &mut i, "°".as_bytes());
    } else {
        push_text(&mut line, &mut i, "传感器离线".as_bytes());
    }
    draw_text(x + 8, y, &line[..i], C_TEXT);

    let orient = unsafe { SENSOR_ORIENT };
    let oname = orientation_name(sensor_client::ScreenOrientation::from_u8(orient));
    let mut line2 = [0u8; 48];
    let mut i2 = 0usize;
    push_text(&mut line2, &mut i2, "方向 ".as_bytes());
    line2[i2..i2 + oname.len()].copy_from_slice(oname);
    i2 += oname.len();
    draw_text(x + display_w() as i32 / 2 - 40, y, &line2[..i2], C_ACCENT);
    y += 24;

    // 步数 + 抬腕。
    let mut line3 = [0u8; 64];
    let mut i3 = 0usize;
    push_text(&mut line3, &mut i3, "步数 ".as_bytes());
    push_dec(&mut line3, &mut i3, unsafe { SENSOR_STEPS } as i64);
    push_text(&mut line3, &mut i3, "  抬腕 ".as_bytes());
    push_dec(&mut line3, &mut i3, unsafe { SENSOR_RAISES } as i64);
    push_text(&mut line3, &mut i3, "  姿态 ".as_bytes());
    push_dec(&mut line3, &mut i3, unsafe { SENSOR_ROLL } as i64);
    push_text(&mut line3, &mut i3, "/".as_bytes());
    push_dec(&mut line3, &mut i3, unsafe { SENSOR_PITCH } as i64);
    draw_text(x + 8, y, &line3[..i3], C_TEXT);
    y += 24;

    // GPS 定位。
    let mut line4 = [0u8; 80];
    let mut i4 = 0usize;
    if unsafe { GPS_VALID } {
        push_text(&mut line4, &mut i4, "定位 N".as_bytes());
        push_dec(&mut line4, &mut i4, unsafe { GPS_LAT } / 100000);
        push_text(&mut line4, &mut i4, "° E".as_bytes());
        push_dec(&mut line4, &mut i4, unsafe { GPS_LON } / 100000);
        push_text(&mut line4, &mut i4, "° 卫".as_bytes());
        push_dec(&mut line4, &mut i4, unsafe { GPS_SATS } as i64);
        draw_text(x + 8, y, &line4[..i4], C_ACCENT);
    } else {
        push_text(&mut line4, &mut i4, "定位: 搜索中…".as_bytes());
        draw_text(x + 8, y, &line4[..i4], C_WARN);
    }
    y += 22;

    // GPS 时间 + 数据源。
    let mut line5 = [0u8; 64];
    let mut i5 = 0usize;
    if unsafe { GPS_VALID } {
        let (h, m, s) = unsafe { GPS_UTC };
        push_text(&mut line5, &mut i5, "UTC ".as_bytes());
        push_2(&mut line5, &mut i5, h);
        push_text(&mut line5, &mut i5, ":".as_bytes());
        push_2(&mut line5, &mut i5, m);
        push_text(&mut line5, &mut i5, ":".as_bytes());
        push_2(&mut line5, &mut i5, s);
    }
    push_text(&mut line5, &mut i5, "  数据源: NMEA 模拟".as_bytes());
    draw_text(x + 8, y, &line5[..i5], C_DIM);
}

/// 无堆的小型文本构造辅助。
fn push_bytes(buf: &mut [u8], i: &mut usize, b: &[u8]) {
    let n = b.len().min(buf.len() - *i);
    buf[*i..*i + n].copy_from_slice(&b[..n]);
    *i += n;
}

fn push_text(buf: &mut [u8], i: &mut usize, s: &[u8]) {
    push_bytes(buf, i, s);
}

fn push_dec(buf: &mut [u8], i: &mut usize, v: i64) {
    if *i >= buf.len() {
        return;
    }
    // 负数支持（传感器姿态/南纬西经为负）：此前负值整段丢弃渲染为空。
    // 经 i128 取绝对值，避免 i64::MIN 溢出。
    let neg = v < 0;
    let mut u = (v as i128).abs();
    let mut tmp = [0u8; 21];
    let mut n = 0usize;
    if u == 0 {
        tmp[0] = b'0';
        n = 1;
    }
    while u > 0 && n < tmp.len() {
        tmp[n] = b'0' + (u % 10) as u8;
        n += 1;
        u /= 10;
    }
    if neg && n < tmp.len() {
        tmp[n] = b'-';
        n += 1;
    }
    while n > 0 && *i < buf.len() {
        n -= 1;
        buf[*i] = tmp[n];
        *i += 1;
    }
}

fn push_2(buf: &mut [u8], i: &mut usize, v: u32) {
    buf[*i] = b'0' + (v / 10) as u8;
    *i += 1;
    buf[*i] = b'0' + (v % 10) as u8;
    *i += 1;
}

fn render() {
    // 液态玻璃基底：垂直渐变背景（替代纯色填充）。
    draw_gradient_bg(C_BG, glass_dark());
    draw_title_bar("地图".as_bytes());

    let zoom = unsafe { ZOOM };

    // 街区（伪随机色块）+ 道路网格
    let mut seed: u32 = 0x9E3779B9;
    let mut r = MAP_TOP;
    while r < MAP_BOTTOM {
        let mut c = 0;
        let mut col = 0;
        while c < display_w() as i32 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let v = (seed >> 24) & 0xF;
            let color = match v {
                0..=7 => C_BLOCK,
                8..=10 => C_PARK,
                11 => C_RIVER,
                _ => C_BLOCK,
            };
            let bw = zoom.saturating_sub(4).min(display_w() as i32 - c);
            let bh = zoom.saturating_sub(4).min(MAP_BOTTOM - r);
            fill_rect(c + 2, r + 2, bw.saturating_sub(4), bh.saturating_sub(4), color);
            // 河流贯穿
            if col == 6 && color != C_RIVER {
                fill_rect(c + 2, r + 2, 12, bh.saturating_sub(4), C_RIVER);
            }
            c += zoom;
            col += 1;
        }
        // 道路横线
        fill_rect(0, r + zoom - 2, display_w() as i32, 4, C_ROAD);
        r += zoom;
    }
    // 道路竖线（在块之上）
    let mut c = zoom;
    while c < display_w() as i32 {
        fill_rect(c - 2, MAP_TOP, 4, MAP_BOTTOM - MAP_TOP, C_ROAD);
        c += zoom;
    }

    // 定位标记（中心）：红色标记上的白色圆心（彩色表面白点保留）。
    let cx = (display_w() / 2) as i32;
    let cy = ((MAP_TOP + MAP_BOTTOM) / 2) as i32;
    draw_disc(cx, cy, 8, C_MARKER);
    draw_circle(cx, cy, 14, C_MARKER);
    draw_disc(cx, cy, 3, 0x00_FF_FF_FF);

    // 罗盘（按航向旋转指针）
    draw_compass(unsafe { SENSOR_HEADING });

    // 缩放按钮
    draw_btn(display_w() as i32 - 64, 64, "+");
    draw_btn(display_w() as i32 - 64, 116, "-");

    // 底部传感器面板（航向 / 方向 / 步数 / 抬腕 / GPS 定位）
    draw_sensor_panel();

    // 底部标签
    draw_text(16, display_h() as i32 - 40, "KunxOS 地图（传感器融合演示）".as_bytes(), C_DIM);

    draw_sys_nav();
}

fn draw_btn(x: i32, y: i32, label: &str) {
    let w = 44;
    let h = 40;
    // 液态玻璃按钮（强调色，几何/坐标不变，仅替换画法）。
    draw_glass_button(x, y, w, h, label.as_bytes(), true);
}

// ==================== 主循环 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[map] 启动中\n");
    if !fb_init() {
        print("[map] /dev/fb0 mmap 失败，无帧缓冲\n");
        unsafe { exit(1); }
    }
    print("[map] 就绪\n");

    let mut dirty = true;
    loop {
        let ev = poll_input();
        if ev.resized { dirty = true; }
        if ev.moved {
            cursor_overlay_move();
        }
        if ev.nav != NavAction::None {
            print("[nav] 系统导航 -> 退出\n");
            unsafe { exit(nav_exit_code(ev.nav)); }
        }

        // 周期性刷新传感器面板（航向 / 步数 / 抬腕 / GPS）。
        refresh_sensor_panel();

        if let Some((x, y)) = ev.tap {
            if back_hit(x, y) {
                print("[map] 返回 -> 退出\n");
                break;
            }
            // 缩放（到达边界时给出提示，此前点击无任何反馈）
            if x >= display_w() as i32 - 64 && x < display_w() as i32 - 20 && y >= 64 && y < 104 {
                let z = unsafe { ZOOM };
                if z < 90 {
                    unsafe { ZOOM = z + 10; }
                    dirty = true;
                } else {
                    show_error("已达最大缩放".as_bytes());
                }
            } else if x >= display_w() as i32 - 64 && x < display_w() as i32 - 20 && y >= 116 && y < 156 {
                let z = unsafe { ZOOM };
                if z > 30 {
                    unsafe { ZOOM = z - 10; }
                    dirty = true;
                } else {
                    show_error("已达最小缩放".as_bytes());
                }
            }
        }
        match ev.key {
            KeyAction::Esc => {
                print("[map] ESC -> 退出\n");
                break;
            }
            _ => {}
        }
        if poll_tty_esc() {
            print("[map] ESC -> 退出\n");
            break;
        }
        if dirty {
            render();
            flip();
            cursor_overlay_redraw();
            dirty = false;
            kit::font_init_checked();
            if kit::font_just_refreshed() { dirty = true; }
        }
        sleep_ms(16);
    }

    print("[map] 正在退出\n");
    unsafe { exit(0); }
}

// ==================== 入口 ====================

global_asm!(
    ".section .text._start, \"ax\"",
    ".global _start",
    ".type _start, %function",
    "_start:",
    "    mv a1, sp",
    "    ld a0, 0(a1)",
    "    addi a1, a1, 8",
    "    call k_main",
    "    li a7, 93",
    "    ecall",
    "    j .",
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
        write_fd(2, b"PANIC: map\n");
        write_fd(2, &w.buf[..w.len]);
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
