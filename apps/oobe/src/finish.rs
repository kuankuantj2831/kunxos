// KunxOS OOBE -> 桌面过渡（finish.rs）。
//
// 点击「进入系统」后由 k_main 调用 enter_desktop()：
//   1. 先把桌面基底帧（壁纸 + 状态栏 + Dock 底条）预绘到显存 —— 进程退出后
//      显存内容保持，init 拉起 launcher 期间屏幕上已经是桌面，消除黑屏空档。
//   2. 叠加「正在进入系统 + 进度条」过渡 UI（点击后 < 100ms 即有视觉反馈）。
//   3. 预加载桌面资源：写入一体化的 desktop_meta（theme/wallpaper/lang，launcher
//      首帧单次读取）、预热字体页缓存。
//   4. 释放 OOBE 非必要内存（关闭输入 fd、清零输入/路径/口令缓冲）。
//   5. 最后写 /data/oobe/.done 并返回，k_main 随即 exit(0)。

const SYS_GETTIMEOFDAY: usize = 169;
const SYS_CLOCK_GETTIME: usize = 113;
const CLOCK_MONOTONIC: i32 = 1;
const SYS_READ: usize = 63;
const O_RDONLY: usize = 0;

const STATUS_H: i32 = 24;
const DOCK_H: i32 = 48;
const DOCK_Y: i32 = crate::DISPLAY_H as i32 - DOCK_H - 56;

const C_TEXT: u32 = 0x00_1A_1C_22;      // 主文字（浅玻璃/白底上）
const C_STATUS_BG: u32 = 0x00_F2_F4_F8; // 状态栏浅色底（备用）
const C_WHITE: u32 = 0x00_FF_FF_FF;
const C_ACCENT: u32 = 0x00_00_B9_6B; // ColorOS 绿（OPPO 绿）
const C_TRACK: u32 = 0x00_E5_E9_F0;  // 进度条轨道（浅灰分割线色）
const C_PANEL: u32 = 0x00_F7_F8_FA;  // 面板浅色底（备用）
const C_PANEL_BORDER: u32 = 0x00_00_B9_6B;

fn now_us() -> u64 {
    // 单调钟：RTC 就绪前后墙钟会跳变，时间差判定（动画/超时）必须用
    // CLOCK_MONOTONIC（黑屏事故同族防御）。
    let mut tv = [0u8; 16];
    unsafe {
        crate::syscall3(SYS_CLOCK_GETTIME, CLOCK_MONOTONIC as usize, tv.as_mut_ptr() as usize, 0);
    }
    let sec = u64::from_le_bytes([tv[0], tv[1], tv[2], tv[3], tv[4], tv[5], tv[6], tv[7]]);
    let nsec = u64::from_le_bytes([tv[8], tv[9], tv[10], tv[11], tv[12], tv[13], tv[14], tv[15]]);
    sec * 1_000_000 + nsec / 1_000
}
fn time_hhmm(buf: &mut [u8; 5]) {
    // 墙钟显示（now_us 已改单调钟）
    let mut tv = [0u8; 16];
    unsafe {
        crate::syscall2(SYS_GETTIMEOFDAY, tv.as_mut_ptr() as usize, 0);
    }
    let us = u64::from_le_bytes([tv[0], tv[1], tv[2], tv[3], tv[4], tv[5], tv[6], tv[7]]) * 1_000_000
        + u64::from_le_bytes([tv[8], tv[9], tv[10], tv[11], tv[12], tv[13], tv[14], tv[15]]);
    let secs = (us / 1_000_000) as u32;
    let total_min = secs / 60;
    let h = (total_min / 60) % 24;
    let m = total_min % 60;
    buf[0] = b'0' + (h / 10) as u8;
    buf[1] = b'0' + (h % 10) as u8;
    buf[2] = b':';
    buf[3] = b'0' + (m / 10) as u8;
    buf[4] = b'0' + (m % 10) as u8;
}

fn blend(a: u32, b: u32, alpha: u32) -> u32 {
    let inv = 255 - alpha.min(255);
    let r = (((a >> 16) & 0xFF) * inv + ((b >> 16) & 0xFF) * alpha) / 255;
    let g = (((a >> 8) & 0xFF) * inv + ((b >> 8) & 0xFF) * alpha) / 255;
    let bl = ((a & 0xFF) * inv + (b & 0xFF) * alpha) / 255;
    (r << 16) | (g << 8) | bl
}

/// 与 launcher draw_background 相同的壁纸渐变（壁纸序号取 OOBE 状态；
/// 系统固定浅色主题，恒用浅色壁纸分支渲染）。
/// `alpha` 为 0..255：从白屏淡入壁纸（ColorOS 15「进入系统」过渡）。
fn draw_wallpaper_a(alpha: u32) {
    let (top, bottom, accent) = unsafe {
        match crate::WALLPAPER {
            0 => (0x00_EF_F6_FF, 0x00_BF_DB_FE, 0x00_60_A5_FA),
            1 => (0x00_F0_FD_FA, 0x00_A7_F3_D0, 0x00_14_B8_A6),
            2 => (0x00_FF_F7_ED, 0x00_FE_D7_AA, 0x00_F9_73_16),
            _ => (0x00_FA_F5_FF, 0x00_E9_D5_FF, 0x00_A8_55_F7),
        }
    };
    let a = alpha.min(255);
    let w = crate::DISPLAY_W as u32;
    let h = crate::DISPLAY_H as u32;
    for y in 0..h {
        let t = y;
        let inv = h - t;
        let r = (((top >> 16) & 0xFF) * inv + ((bottom >> 16) & 0xFF) * t) / h;
        let g = (((top >> 8) & 0xFF) * inv + ((bottom >> 8) & 0xFF) * t) / h;
        let b = ((top & 0xFF) * inv + (bottom & 0xFF) * t) / h;
        let row = (r << 16) | (g << 8) | b;
        for x in 0..w {
            let wave = if ((x / 72 + y / 96) & 1) == 0 { accent } else { row };
            let color = if x % 72 < 2 && y > 180 { blend(row, wave, 26) } else { row };
            let c = if a >= 255 { color } else { blend(0x00_FF_FF_FF, color, a) };
            crate::put_pixel(x as i32, y as i32, c);
        }
    }
}

fn draw_wallpaper() {
    draw_wallpaper_a(255);
}

fn ease_out(t: f32) -> f32 {
    if t <= 0.0 {
        0.0
    } else if t >= 1.0 {
        1.0
    } else {
        1.0 - (1.0 - t) * (1.0 - t)
    }
}

/// ColorOS 15 桌面淡入：壁纸从白屏淡入（先快后缓，~360ms），
/// 与 OOBE 完成页的绿徽标同节奏。
fn wallpaper_fade_in() {
    for f in 0..16 {
        let t = f as f32 / 15.0;
        draw_wallpaper_a((255.0 * ease_out(t)) as u32);
        crate::flip();
        crate::sleep_ms(15);
    }
}

fn draw_wifi(x: i32, y: i32, color: u32) {
    // Phosphor wifi-high 矢量字形（抗锯齿；替换逐像素圆弧）。
    if crate::draw_icon_thick(x + 7, y + 7, ttf_cp::WIFI, 14.0, color) {
        return;
    }
    let cx = x + 7;
    let cy = y + 11;
    crate::put_pixel(cx, cy, color);
    crate::put_pixel(cx + 1, cy, color);
    crate::put_pixel(cx, cy + 1, color);
    crate::put_pixel(cx + 1, cy + 1, color);
    let mut r = 3;
    while r <= 7 {
        let mut dx = -r;
        while dx <= r {
            let dy = crate::isqrt(r * r - dx * dx);
            crate::put_pixel(cx + dx, cy - dy, color);
            dx += 1;
        }
        r += 2;
    }
}

fn draw_battery(x: i32, y: i32, color: u32) {
    // Phosphor battery-full 矢量字形（抗锯齿；替换硬边矩形拼合）。
    if crate::draw_icon_thick(x + 9, y + 5, ttf_cp::BATT, 14.0, color) {
        return;
    }
    crate::draw_rect(x, y, 18, 10, color);
    crate::fill_rect(x + 18, y + 3, 2, 4, color);
    crate::fill_rect(x + 2, y + 2, 12, 6, color);
}

/// shared/icons.rs 码点别名（收窄本文件引用长度）。
mod ttf_cp {
    pub(crate) use crate::ttf_text::font_render::icons::settings::WIFI as WIFI;
    pub(crate) use crate::ttf_text::font_render::icons::ui::BATTERY as BATT;
}

fn draw_status_bar() {
    // 液态玻璃状态栏（与 launcher 一致：浅色玻璃 + 深色文字）。
    crate::draw_glass(0, 0, crate::DISPLAY_W as i32, STATUS_H, 0);
    let mut hhmm = [0u8; 5];
    time_hhmm(&mut hhmm);
    crate::draw_text(8, (STATUS_H - 8) / 2, &hhmm, C_TEXT);
    draw_wifi(486, (STATUS_H - 14) / 2, C_TEXT);
    draw_battery(512, (STATUS_H - 10) / 2, C_TEXT);
}

fn draw_dock_strip() {
    // 悬浮液态玻璃 Dock（与 launcher 的浅色玻璃药丸一致）。
    crate::draw_glass(12, DOCK_Y + 2, crate::DISPLAY_W as i32 - 24, DOCK_H - 4, 30);
}

fn pct_text(pct: usize, out: &mut [u8; 8]) -> usize {
    let mut n = 0usize;
    if pct >= 100 {
        out[n] = b'1';
        n += 1;
        out[n] = b'0';
        n += 1;
        out[n] = b'0';
        n += 1;
    } else if pct >= 10 {
        out[n] = b'0' + (pct / 10) as u8;
        n += 1;
        out[n] = b'0' + (pct % 10) as u8;
        n += 1;
    } else {
        out[n] = b'0' + pct as u8;
        n += 1;
    }
    out[n] = b'%';
    n + 1
}

/// 过渡 UI：液态玻璃进度面板（位于 Dock 上方，launcher 启动后在同一位置接管指示器）。
fn draw_progress_overlay(pct: usize) {
    let bw = 240i32;
    let bx = (crate::DISPLAY_W as i32 - bw) / 2;
    let by = DOCK_Y - 56;
    crate::draw_glass(bx - 10, by, bw + 20, 52, 16);
    let label: &[u8] = if unsafe { crate::LANG == 0 } {
        "正在进入系统".as_bytes()
    } else {
        b"Entering system"
    };
    let lw = crate::text_width(label);
    crate::draw_text(bx + (bw - lw) / 2, by + 4, label, C_TEXT);
    crate::fill_rect(bx, by + 26, bw, 10, C_TRACK);
    crate::fill_rect(bx, by + 26, (bw * pct as i32) / 100, 10, C_ACCENT);
    let mut ps = [0u8; 8];
    let pn = pct_text(pct, &mut ps);
    let pw = crate::text_width(&ps[..pn]);
    crate::draw_text(bx + bw - pw, by + 38, &ps[..pn], C_TEXT);
}

fn write_desktop_meta() {
    let mut meta = [0u8; 24];
    let mut n = 0usize;
    let mode: &[u8] = if unsafe { crate::THEME_DARK } { b"dark" } else { b"light" };
    meta[..mode.len()].copy_from_slice(mode);
    n += mode.len();
    meta[n] = b',';
    n += 1;
    meta[n] = b'w';
    n += 1;
    meta[n] = b':';
    n += 1;
    meta[n] = b'0' + unsafe { crate::WALLPAPER } as u8;
    n += 1;
    meta[n] = b',';
    n += 1;
    let lang: &[u8] = if unsafe { crate::LANG == 0 } { b"zh" } else { b"en" };
    meta[n..n + lang.len()].copy_from_slice(lang);
    n += lang.len();
    crate::write_file("/data/oobe/desktop_meta", &meta[..n]);
}

/// 预热字体页缓存：OOBE 启动时已完整读入 /fonts/cn.ttf（font_init），
/// 此处再触碰首页，确保 launcher 读字体时页缓存命中。
fn warm_font_cache() {
    unsafe {
        let fd = crate::open("/fonts/cn.ttf", O_RDONLY);
        if fd >= 0 {
            let mut buf = [0u8; 4096];
            crate::syscall3(crate::SYS_READ, fd as usize, buf.as_mut_ptr() as usize, buf.len());
            crate::syscall1(crate::SYS_CLOSE, fd as usize);
        }
    }
}

/// 释放 OOBE 非必要内存：关闭输入 fd、清零输入累积 / 路径 / 口令缓冲。
/// 进程退出后内核回收其余全部内存（含 2MB back buffer）。
fn release_resources() {
    unsafe {
        if crate::INPUT_FD >= 0 {
            crate::syscall1(crate::SYS_CLOSE, crate::INPUT_FD as usize);
            crate::INPUT_FD = -1;
        }
        crate::EV_LEN = 0;
        for b in crate::EV_BUF.iter_mut() {
            *b = 0;
        }
        for b in crate::PATH_BUF.iter_mut() {
            *b = 0;
        }
        for b in crate::USER_BUF.iter_mut() {
            *b = 0;
        }
        for b in crate::PASS_BUF.iter_mut() {
            *b = 0;
        }
        for b in crate::CONFIRM_BUF.iter_mut() {
            *b = 0;
        }
        for b in crate::DURESS_BUF.iter_mut() {
            *b = 0;
        }
        for b in crate::DURESS_CONFIRM_BUF.iter_mut() {
            *b = 0;
        }
    }
}

/// 完成页点击「进入系统」后的过渡入口。
pub fn enter_desktop() {
    crate::print("[oobe] 进入桌面（预加载）\n");
    let fb_ok = unsafe { !crate::FB_PTR.is_null() };

    // 1. 桌面基底帧：壁纸从白屏淡入（ColorOS 15 过渡），随后状态栏 + Dock 底条上屏。
    if fb_ok {
        wallpaper_fade_in();
        draw_wallpaper();
        draw_status_bar();
        draw_dock_strip();
        crate::flip();
        // 2. 过渡 UI（进度 0%）。
        draw_progress_overlay(0);
        crate::flip();
    }

    // 3. 资源预加载（每步有界，主线程单步 < 100ms）。
    if fb_ok {
        draw_progress_overlay(30);
        crate::flip();
    }
    write_desktop_meta();
    crate::print("[oobe] 桌面元数据已写入\n");

    if fb_ok {
        draw_progress_overlay(55);
        crate::flip();
    }
    warm_font_cache();
    crate::print("[oobe] 字体页缓存已预热\n");

    if fb_ok {
        draw_progress_overlay(80);
        crate::flip();
    }
    release_resources();
    crate::print("[oobe] 资源已释放\n");

    // 4. 完成标志最后写：init 在 wait4 返回后检查 .done。
    if fb_ok {
        draw_progress_overlay(100);
        crate::flip();
        crate::sleep_ms(60);
    }
    crate::write_file("/data/oobe/.done", b"1");
    crate::print("[oobe] /data/oobe/.done 已创建\n");
}
