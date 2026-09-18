//! KunxOS 桌面应用公共框架（EL0 用户态，裸 syscall、no_std）。
//!
//! 各应用以 `#[path = "../../../shared/app_kit.rs"] mod kit;` 引入本模块，
//! 再用 `use kit::*;` 使用公共能力：系统调用封装、framebuffer 初始化与双缓冲、
//! 绘制原语（像素/矩形/线/圆）、TTF 平滑字体渲染（常规 cn.ttf + 标题 bold.ttf，
//! 见 `ttf_text`）、鼠标/触摸/键盘输入、光标、标题栏与返回键、墙钟时间与公历换算。
//!
//! 约定：
//! - 应用入口：`k_main(argc, argv)`，内部自行 `global_asm!` 定义 `_start`
//!   蹦床（见 stub 应用示例）。
//! - 分辨率 540x960，每应用独占全屏；返回键 / ESC 退出后由 launcher 重绘桌面。
//! - 文本渲染使用 cn.ttf 平滑字体：首次绘制前调用 `ttf_text::font_init()`
//!   （字体未就绪时文本渲染为空操作）。

use core::arch::asm;

#[path = "ttf_text.rs"]
pub mod ttf_text;

#[path = "nav.rs"]
pub mod nav;

pub use nav::{NavAction, NavCanvas, NAV_Y};

/// 应用生命周期客户端（lifd 事件信箱 + SIGSTOP/SIGCONT 配合）。
/// 见 `docs/lifecycle.md`。`poll_input` 已自动调用 `lc_poll`，
/// 应用只需把 `exit(nav_exit_code(ev.nav))` 换成 `lc_handle_nav(ev.nav)`。
#[path = "lifecycle.rs"]
pub mod lifecycle;

#[allow(unused_imports)] // 供各应用按需 glob 引入；未被引用的应用会误报
pub use lifecycle::{
    lc_clear_redraw, lc_end_bg_task, lc_go_recents, lc_handle_nav, lc_home, lc_init,
    lc_needs_redraw, lc_notify_local, lc_poll, lc_request_bg_task, lc_restore_state,
    lc_save_state, lc_set_callbacks, lc_set_name, BGT_AUDIO, BGT_DOWNLOAD, BGT_LOCATION,
    BGT_VOIP, LcCallbacks, LcEvent,
};

/// 共享毛玻璃 / 液态玻璃渲染（由 nav.rs 引入，此处再导出给应用与 ds.rs）。
pub use nav::glass;
pub use nav::glass::*;

// ==================== 毛玻璃辅助（针对本进程 BACK_BUF） ====================

/// 当前玻璃深浅模式：全系统固定浅色（应用白色背景打底，不再读取主题文件）。
pub fn glass_dark() -> bool {
    false
}

/// 以本进程 BACK_BUF 为背景的毛玻璃目标（读背景 + 写 put_pixel）。
pub struct BufGlass;

impl glass::GlassTarget for BufGlass {
    fn backdrop(&self, x: i32, y: i32) -> u32 {
        if x < 0 || y < 0 || x >= DISPLAY_W as i32 || y >= DISPLAY_H as i32 {
            0
        } else {
            unsafe { BACK_BUF[y as usize * DISPLAY_W + x as usize] }
        }
    }
    fn put(&mut self, x: i32, y: i32, color: u32) {
        put_pixel(x, y, color);
    }
}

/// 绘制毛玻璃面板（深/浅色按主题自动选择）。
pub fn draw_glass(x: i32, y: i32, w: i32, h: i32, r: i32) {
    glass_rect(&mut BufGlass, x, y, w, h, r, glass_dark(), true);
}

/// 按自定义样式绘制液态玻璃面板。
pub fn draw_glass_style(x: i32, y: i32, w: i32, h: i32, r: i32, st: &GlassStyle) {
    glass_rect_style(&mut BufGlass, x, y, w, h, r, st, true);
}

/// 液态玻璃基底：垂直渐变背景（顶部提亮、底部压暗同色系），让面板玻璃有
/// 可模糊的内容层次。应用在整屏背景填充处调用以替代纯色填充。
pub fn draw_gradient_bg(color: u32, dark: bool) {
    let cr = ((color >> 16) & 0xFF) as u32;
    let cg = ((color >> 8) & 0xFF) as u32;
    let cb = (color & 0xFF) as u32;
    let (lift, sink) = if dark { (0, 22) } else { (18, 14) };
    let tr = (cr + lift).min(255);
    let tg = (cg + lift).min(255);
    let tb = (cb + lift).min(255);
    let br = cr.saturating_sub(sink);
    let bg_ = cg.saturating_sub(sink);
    let bb = cb.saturating_sub(sink);
    for y in 0..DISPLAY_H as i32 {
        let t = (y as u32 * 255 / DISPLAY_H as u32).min(255);
        let inv = 255 - t;
        let r = (tr * inv + br * t) / 255;
        let g = (tg * inv + bg_ * t) / 255;
        let b = (tb * inv + bb * t) / 255;
        let row = (r << 16) | (g << 8) | b;
        let iy = y as usize * DISPLAY_W;
        unsafe {
            for x in 0..DISPLAY_W {
                BACK_BUF[iy + x] = row;
            }
        }
    }
}

/// 玻璃按钮：液态玻璃底 + 居中标签；accent = 强调色玻璃（全胶囊圆角，ColorOS 15 风格）。
pub fn draw_glass_button(x: i32, y: i32, w: i32, h: i32, label: &[u8], accent: bool) {
    let r = h / 2;
    if accent {
        draw_glass_style(x, y, w, h, r, &STYLE_ACCENT);
    } else {
        draw_glass(x, y, w, h, r);
    }
    // 强调色玻璃上用白字，浅色玻璃上用深色字。
    let label_col = if accent { 0x00_FF_FF_FF } else { C_TEXT };
    let tw = text_width(label);
    draw_text(x + (w - tw) / 2, y + (h - 20) / 2, label, label_col);
}

/// 玻璃键盘按键（圆角玻璃方块 + 标签；30% 圆角，ColorOS 15 键盘观感）。
pub fn draw_glass_key(x: i32, y: i32, w: i32, h: i32, label: &[u8]) {
    let r = (h * 30 / 100).min(16);
    draw_glass(x, y, w, h, r);
    let tw = text_width(label);
    draw_text(x + (w - tw) / 2, y + (h - 20) / 2, label, C_TEXT);
}

#[path = "error_ui.rs"]
pub mod error_ui;

/// 设置一条用户可见的错误提示（自动在 `ERR_DURATION_US` 后消失）。
pub fn show_error(msg: &[u8]) {
    error_ui::err_set(msg, now_us());
}

/// 清除当前错误提示。
pub fn clear_error() {
    error_ui::err_clear();
}

/// 初始化 TTF 字体并检查结果：失败时（字体缺失/解析错误）记录错误横幅
/// 并打印串口诊断——此前调用方忽略返回值，全部文字会静默消失。
///
/// 该函数仅作一次性同步加载；调用方可用 [`font_just_refreshed`] 判定本调用
/// 是否让字体从「未就绪」变为「就绪」，从而立即补一帧平滑字体的重绘。
pub fn font_init_checked() {
    let was = ttf_text::font_render::font_ready();
    if !ttf_text::font_init() {
        print("[kit] WARNING：字体初始化失败，文字将无法渲染\n");
        show_error("字体加载失败".as_bytes());
    }
    let now = ttf_text::font_render::font_ready();
    unsafe { FONT_JUST_REFRESHED = !was && now; }
}

static mut FONT_JUST_REFRESHED: bool = false;

/// 消费一次性「字体刚就绪」标记：返回 true 表示本次启动中字体由未就绪变为
/// 就绪（调用方应补一次整帧重绘以切换到平滑字体），随后自动清除。
pub fn font_just_refreshed() -> bool {
    let r = unsafe { FONT_JUST_REFRESHED };
    unsafe { FONT_JUST_REFRESHED = false; }
    r
}

// ==================== 系统调用号（AArch64 asm-generic） ====================

pub const SYS_READ: usize = 63;
pub const SYS_WRITE: usize = 64;
pub const SYS_OPENAT: usize = 56;
pub const SYS_CLOSE: usize = 57;
pub const SYS_IOCTL: usize = 29;
pub const SYS_EXIT: usize = 93;
pub const SYS_NANOSLEEP: usize = 101;
pub const SYS_MMAP: usize = 222;
pub const SYS_GETTIMEOFDAY: usize = 169;
pub const SYS_GETUID: usize = 174;
pub const SYS_SOCKET: usize = 198;
pub const SYS_CONNECT: usize = 203;
pub const SYS_SENDTO: usize = 206;
pub const SYS_RECVFROM: usize = 207;
pub const SYS_UNLINKAT: usize = 35;
pub const SYS_RENAMEAT: usize = 38;
pub const SYS_FSYNC: usize = 82;
pub const SYS_SHUTDOWN: usize = 210;
pub const SYS_DNS_RESOLVE: usize = 403;
/// 自定义：内核内存压力等级（0/1/2），见 `mem_pressure` 模块。
pub const SYS_LC_MEMPRESSURE: usize = 404;

/// 生命周期用系统调用（Linux asm-generic 编号）。
pub const SYS_GETPID: usize = 172;
pub const SYS_KILL: usize = 129;
pub const SYS_SETPRIORITY: usize = 140;

/// AF_INET（AArch64 Linux 常量）。
pub const AF_INET: usize = 2;
pub const SOCK_STREAM: usize = 1;

/// 打开一个 TCP 套接字（内核 socket.rs）。失败返回负值。
pub unsafe fn sock_open() -> isize {
    syscall3(SYS_SOCKET, AF_INET, SOCK_STREAM, 0) as isize
}

/// TCP 连接到 `ip:port`（ip 为主机字节序 u32）。失败返回负值。
pub unsafe fn sock_connect(fd: isize, ip: u32, port: u16) -> isize {
    let mut sa = [0u8; 8];
    sa[0] = AF_INET as u8;
    sa[1] = 0;
    sa[2] = (port >> 8) as u8;
    sa[3] = (port & 0xFF) as u8;
    sa[4] = (ip >> 24) as u8;
    sa[5] = ((ip >> 16) & 0xFF) as u8;
    sa[6] = ((ip >> 8) & 0xFF) as u8;
    sa[7] = (ip & 0xFF) as u8;
    syscall2(SYS_CONNECT, fd as usize, sa.as_ptr() as usize) as isize
}

/// TCP 发送（阻塞直到 ACK）。失败返回负值。
pub unsafe fn sock_send(fd: isize, data: &[u8]) -> isize {
    syscall6(
        SYS_SENDTO,
        fd as usize,
        data.as_ptr() as usize,
        data.len(),
        0,
        0,
        0,
    ) as isize
}

/// 关闭 TCP 套接字（注意：必须用 SYS_SHUTDOWN，不能用 SYS_CLOSE ——
/// 后者关闭的是进程 fd 表里的文件描述符，socket 句柄是独立空间）。
pub unsafe fn sock_close(fd: isize) {
    syscall1(SYS_SHUTDOWN, fd as usize);
}

/// DNS 解析域名（内核经 QEMU slirp → 宿主 DNS）。返回主机字节序 IPv4，0 = 失败。
pub unsafe fn dns_resolve(name: &[u8]) -> u32 {
    let mut buf = [0u8; 256];
    let n = name.len().min(buf.len() - 1);
    buf[..n].copy_from_slice(&name[..n]);
    buf[n] = 0;
    syscall1(SYS_DNS_RESOLVE, buf.as_ptr() as usize) as u32
}

/// TCP 接收（阻塞直到有数据或对端关闭；对端关闭返回 0）。失败返回负值。
pub unsafe fn sock_recv(fd: isize, buf: &mut [u8]) -> isize {
    syscall6(
        SYS_RECVFROM,
        fd as usize,
        buf.as_mut_ptr() as usize,
        buf.len(),
        0,
        0,
        0,
    ) as isize
}

/// 解析点分十进制 IPv4 为 u32（主机字节序）。失败返回 None。
pub fn ip_parse(s: &[u8]) -> Option<u32> {
    let mut parts = [0u32; 4];
    let mut pi = 0usize;
    let mut acc: u32 = 0;
    let mut digits = false;
    for &c in s {
        if c >= b'0' && c <= b'9' {
            acc = acc * 10 + (c - b'0') as u32;
            if acc > 255 {
                return None;
            }
            digits = true;
        } else if c == b'.' {
            if !digits || pi >= 3 {
                return None;
            }
            parts[pi] = acc;
            pi += 1;
            acc = 0;
            digits = false;
        } else {
            return None;
        }
    }
    if !digits || pi != 3 {
        return None;
    }
    parts[3] = acc;
    Some((parts[0] << 24) | (parts[1] << 16) | (parts[2] << 8) | parts[3])
}

// ==================== 常量 ====================

pub const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
pub const O_RDONLY: usize = 0;
pub const O_WRONLY: usize = 1;
pub const O_RDWR: usize = 2;
pub const O_NONBLOCK: usize = 0x800;

pub const PROT_READ: usize = 1;
pub const PROT_WRITE: usize = 2;
pub const MAP_SHARED: usize = 1;

/// FBIOGET_VSCREENINFO 请求码（与内核 devfs 约定一致）。
pub const FBIOGET_VSCREENINFO: usize = 0x4600;
/// FBIO_PUT_DAMAGE：提交变更矩形（应用直绘脏区域追踪，仅登记不呈现）。
pub const FBIO_PUT_DAMAGE: usize = 0x4602;
/// FBIO_PRESENT：提交变更矩形并请求页翻转（应用直绘呈现）。
pub const FBIO_PRESENT: usize = 0x4603;

/// 单帧脏矩形上限（与内核 MAX_DIRTY 一致；超限退化为整屏）。
pub const MAX_DIRTY: usize = 32;

pub const FB_PATH: &str = "/dev/fb0";
pub const INPUT_PATH: &str = "/dev/input/event0";

/// linux input_event 事件类型 / 码。
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const EV_ABS: u16 = 0x03;
pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const REL_X: u16 = 0x00;
pub const REL_Y: u16 = 0x01;
pub const BTN_TOUCH: u16 = 0x14a; // 330
pub const BTN_LEFT: u16 = 0x110; // 272

/// evdev 键码（QWERTY 物理布局编号）。
pub const KEY_ENTER: u16 = 28;
pub const KEY_BACKSPACE: u16 = 14;
pub const KEY_ESC: u16 = 1;
pub const KEY_LEFTSHIFT: u16 = 42;
pub const KEY_RIGHTSHIFT: u16 = 54;

// ==================== 显示参数 ====================

pub const DISPLAY_W: usize = 540;
pub const DISPLAY_H: usize = 960;
pub const TITLE_H: i32 = 44;
pub const BACK_SZ: i32 = 36;
pub const BACK_X: i32 = 4;
pub const BACK_Y: i32 = 4;

// ==================== 默认配色（应用可自行覆盖） ====================

pub const C_TITLE_BG: u32 = 0x00_F2_F4_F8;
pub const C_TEXT: u32 = 0x00_1A_1C_22;
pub const C_DIM: u32 = 0x00_6B_72_80;
pub const C_BTN: u32 = 0x00_FF_FF_FF;
pub const C_BTN_BORDER: u32 = 0x00_D9_DE_E6;
pub const C_ACCENT: u32 = 0x00_00_B9_6B;

// ==================== 全局状态 ====================

pub static mut FB_PTR: *mut u32 = core::ptr::null_mut();
pub static mut FB_W: usize = DISPLAY_W;
pub static mut FB_H: usize = DISPLAY_H;
pub static mut FB_STRIDE_PX: usize = DISPLAY_W;
pub static mut BACK_BUF: [u32; DISPLAY_W * DISPLAY_H] = [0u32; DISPLAY_W * DISPLAY_H];

// ---- 脏区域追踪（应用直绘 → 内核脏区域合成器） ----
/// 保持打开的 /dev/fb0 fd（ioctl 提交脏区/请求呈现用；mmap 后可关闭映射，
/// 但 fd 需保留到进程退出）。
pub static mut FB_FD: isize = -1;
/// 本帧累计的脏矩形（mark_dirty 合并；超限置 FULL）。
pub static mut DIRTY_FULL: bool = false;
pub static mut DIRTY_COUNT: usize = 0;
pub static mut DIRTY: [(i32, i32, u32, u32); MAX_DIRTY] = [(0, 0, 0, 0); MAX_DIRTY];
/// FBIO_PRESENT 用户缓冲区（count + rects，与内核 devfs 约定布局一致）。
static mut DAMAGE_BUF: [u8; 4 + MAX_DIRTY * 16] = [0u8; 4 + MAX_DIRTY * 16];

/// 标记一个变更矩形（合并重叠/相邻；超限退化为整屏）。
///
/// 应用在重绘到 BACK_BUF 后、`flip` 前对每个变更区域调用；`flip` 只把
/// 这些矩形从 BACK_BUF 拷贝到显存并提交给内核，替代整屏 2MB memcpy。
pub fn mark_dirty(x: i32, y: i32, w: u32, h: u32) {
    unsafe {
        if w == 0 || h == 0 {
            return;
        }
        if DIRTY_FULL {
            return;
        }
        let r = (x, y, w, h);
        for i in 0..DIRTY_COUNT {
            let d = DIRTY[i];
            if rects_intersect(d, r) {
                let nx = d.0.min(x);
                let ny = d.1.min(y);
                let nr = (d.0 + d.2 as i32).max(x + w as i32);
                let nb = (d.1 + d.3 as i32).max(y + h as i32);
                DIRTY[i] = (nx, ny, (nr - nx) as u32, (nb - ny) as u32);
                return;
            }
        }
        if DIRTY_COUNT >= MAX_DIRTY {
            DIRTY_FULL = true;
            return;
        }
        DIRTY[DIRTY_COUNT] = r;
        DIRTY_COUNT += 1;
    }
}

/// 两个矩形是否相交（含相邻右/下边界）。
fn rects_intersect(a: (i32, i32, u32, u32), b: (i32, i32, u32, u32)) -> bool {
    a.0 < b.0 + b.2 as i32 && b.0 < a.0 + a.2 as i32 && a.1 < b.1 + b.3 as i32 && b.1 < a.1 + a.3 as i32
}

/// 提交脏区并请求呈现：把 dirty 列表编码进 DAMAGE_BUF，调用 FBIO_PRESENT。
fn fb_submit_present() {
    unsafe {
        if FB_FD < 0 || FB_PTR.is_null() {
            DIRTY_FULL = false;
            DIRTY_COUNT = 0;
            return;
        }
        if DIRTY_FULL || DIRTY_COUNT == 0 {
            // 整屏：单矩形 (0, 0, FB_W, FB_H)
            DAMAGE_BUF[0..4].copy_from_slice(&1u32.to_le_bytes());
            DAMAGE_BUF[4..8].copy_from_slice(&0i32.to_le_bytes());
            DAMAGE_BUF[8..12].copy_from_slice(&0i32.to_le_bytes());
            DAMAGE_BUF[12..16].copy_from_slice(&(FB_W as u32).to_le_bytes());
            DAMAGE_BUF[16..20].copy_from_slice(&(FB_H as u32).to_le_bytes());
        } else {
            DAMAGE_BUF[0..4].copy_from_slice(&(DIRTY_COUNT as u32).to_le_bytes());
            for i in 0..DIRTY_COUNT {
                let d = DIRTY[i];
                let off = 4 + i * 16;
                DAMAGE_BUF[off..off + 4].copy_from_slice(&d.0.to_le_bytes());
                DAMAGE_BUF[off + 4..off + 8].copy_from_slice(&d.1.to_le_bytes());
                DAMAGE_BUF[off + 8..off + 12].copy_from_slice(&d.2.to_le_bytes());
                DAMAGE_BUF[off + 12..off + 16].copy_from_slice(&d.3.to_le_bytes());
            }
        }
        // 忽略返回值：内核在应用接管模式下始终呈现（无脏区时为 no-op）
        syscall3(SYS_IOCTL, FB_FD as usize, FBIO_PRESENT, DAMAGE_BUF.as_ptr() as usize);
        DIRTY_FULL = false;
        DIRTY_COUNT = 0;
    }
}

/// 把 BACK_BUF 的 `r` 矩形拷贝到显存（行内整行拷贝，越界裁剪）。
fn copy_dirty_rect(r: (i32, i32, u32, u32)) {
    unsafe {
        let x0 = r.0.max(0);
        let y0 = r.1.max(0);
        // ★ 源缓冲按 DISPLAY_W/H 布局，目标按 FB_W/FB_H：两边都要钳制，
        // 否则内核报告的 FB 尺寸大于应用假设的 DISPLAY 尺寸时，
        // src 指针读越界（裸指针静默 UB，读进相邻静态区）。
        let x1 = (r.0 + r.2 as i32).min(FB_W as i32).min(DISPLAY_W as i32);
        let y1 = (r.1 + r.3 as i32).min(FB_H as i32).min(DISPLAY_H as i32);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let w = (x1 - x0) as usize;
        for y in y0..y1 {
            let src = BACK_BUF.as_ptr().add(y as usize * DISPLAY_W + x0 as usize);
            let dst = FB_PTR.add(y as usize * FB_STRIDE_PX + x0 as usize);
            core::ptr::copy_nonoverlapping(src, dst, w);
        }
    }
}

/// 鼠标/触摸指针状态。
pub static mut CUR_X: i32 = DISPLAY_W as i32 / 2;
pub static mut CUR_Y: i32 = DISPLAY_H as i32 / 2;
pub static mut MOUSE_DOWN: bool = false;
pub static mut TOUCH_DOWN: bool = false;
pub static mut SHIFT: bool = false;
pub static mut INPUT_FD: isize = -1;
/// 输入设备缺失提示是否已展示（恢复后用于清除横幅）。
pub static mut INPUT_ERR_SHOWN: bool = false;

/// 触摸手势追踪：按下点与时刻（用于系统导航手势分类）。
pub static mut TOUCH_PX: i32 = 0;
pub static mut TOUCH_PY: i32 = 0;
pub static mut TOUCH_US: u64 = 0;

/// 光随指动缓动是否有新帧（读后自清；应用据此置 dirty 重绘玻璃高光）。
static mut LIGHT_ANIM: bool = false;

/// 光随指动是否在缓动中（读后自清；应用据此置 dirty）。
pub fn light_animating() -> bool {
    unsafe {
        let a = LIGHT_ANIM;
        LIGHT_ANIM = false;
        a
    }
}

// ==================== 系统调用封装 ====================

#[inline(always)]
pub unsafe fn syscall1(n: usize, a1: usize) -> usize {
    let ret: usize;
    asm!("svc #0", in("x8") n, inlateout("x0") a1 => ret, options(nostack));
    ret
}

#[inline(always)]
pub unsafe fn syscall2(n: usize, a1: usize, a2: usize) -> usize {
    let ret: usize;
    asm!("svc #0", in("x8") n, inlateout("x0") a1 => ret, in("x1") a2, options(nostack));
    ret
}

#[inline(always)]
pub unsafe fn syscall3(n: usize, a1: usize, a2: usize, a3: usize) -> usize {
    let ret: usize;
    asm!(
        "svc #0",
        in("x8") n,
        inlateout("x0") a1 => ret,
        in("x1") a2,
        in("x2") a3,
        options(nostack)
    );
    ret
}

/// renameat / fsync 等四参系统调用（文件信箱原子提交用）。
#[inline(always)]
pub unsafe fn syscall4(n: usize, a1: usize, a2: usize, a3: usize, a4: usize) -> usize {
    let ret: usize;
    asm!(
        "svc #0",
        in("x8") n,
        inlateout("x0") a1 => ret,
        in("x1") a2,
        in("x2") a3,
        in("x3") a4,
        options(nostack)
    );
    ret
}

#[inline(always)]
pub unsafe fn syscall6(
    n: usize,
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
    a5: usize,
    a6: usize,
) -> usize {
    let ret: usize;
    asm!(
        "svc #0",
        in("x8") n,
        inlateout("x0") a1 => ret,
        in("x1") a2,
        in("x2") a3,
        in("x3") a4,
        in("x4") a5,
        in("x5") a6,
        options(nostack)
    );
    ret
}

// ==================== 基础 I/O ====================

pub unsafe fn write_fd(fd: usize, buf: &[u8]) -> usize {
    syscall3(SYS_WRITE, fd, buf.as_ptr() as usize, buf.len())
}

pub fn print(s: &str) {
    unsafe {
        write_fd(1, s.as_bytes());
    }
}

pub fn print_dec(mut v: u64) {
    if v == 0 {
        print("0");
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    unsafe {
        write_fd(1, &buf[i..]);
    }
}

pub fn sleep_ms(ms: usize) {
    let mut req = [0u8; 16];
    let sec = (ms / 1000) as u64;
    let nsec = ((ms % 1000) * 1_000_000) as u64;
    req[..8].copy_from_slice(&sec.to_le_bytes());
    req[8..].copy_from_slice(&nsec.to_le_bytes());
    unsafe {
        syscall2(SYS_NANOSLEEP, req.as_ptr() as usize, 0);
    }
}

pub unsafe fn exit(code: usize) -> ! {
    syscall1(SYS_EXIT, code);
    loop {}
}

static mut PATH_BUF: [u8; 128] = [0u8; 128];

pub unsafe fn open(path: &str, flags: usize) -> isize {
    let n = path.len().min(PATH_BUF.len() - 1);
    PATH_BUF[..n].copy_from_slice(&path.as_bytes()[..n]);
    PATH_BUF[n] = 0;
    syscall3(SYS_OPENAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, flags) as isize
}

/// 当前墙钟时间（微秒，epoch）。内核 RTC 提供 UTC 墙钟。
/// 墙钟不可用（gettimeofday 失败）时返回 0——调用方按"未知时间"处理，
/// 而不是读到未初始化的栈垃圾。
pub fn now_us() -> u64 {
    let mut tv = [0u8; 16];
    unsafe {
        let r = syscall2(SYS_GETTIMEOFDAY, tv.as_mut_ptr() as usize, 0);
        if (r as isize) < 0 {
            return 0;
        }
        let sec = u64::from_le_bytes([tv[0], tv[1], tv[2], tv[3], tv[4], tv[5], tv[6], tv[7]]);
        let usec =
            u64::from_le_bytes([tv[8], tv[9], tv[10], tv[11], tv[12], tv[13], tv[14], tv[15]]);
        sec * 1_000_000 + usec
    }
}

// ==================== 公历换算（Howard Hinnant 算法） ====================

/// epoch 天数 → 公历 (年, 月, 日)，月/日 1 基。
pub fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y } as i32;
    (y, m, d)
}

/// 公历 (年, 月, 日) → epoch 天数；月/日 1 基。
pub fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y } as i64;
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m + 9) % 12; // [0, 11]
    let doy = ((153 * mp + 2) / 5) as i64 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// epoch 天数 → 星期（0=周日, 1=周一, …, 6=周六）。
pub fn weekday_from_days(days: i64) -> u32 {
    let w = (days + 4) % 7;
    if w < 0 {
        (w + 7) as u32
    } else {
        w as u32
    }
}

/// 当前本地时间（由墙钟 epoch 换算，UTC）。
pub fn now_hms() -> (u32, u32, u32) {
    let us = now_us();
    let secs = (us / 1_000_000) as u64;
    let h = ((secs / 3600) % 24) as u32;
    let m = ((secs / 60) % 60) as u32;
    let s = (secs % 60) as u32;
    (h, m, s)
}

/// 当前公历日期（UTC）。
pub fn now_date() -> (i32, u32, u32) {
    let days = (now_us() / 1_000_000 / 86400) as i64;
    civil_from_days(days)
}

// ==================== 帧缓冲 ====================

pub fn fb_init() -> bool {
    unsafe {
        let fd = open(FB_PATH, O_RDWR);
        if fd < 0 {
            return false;
        }
        let mut info = [0u8; 24];
        let r = syscall3(
            SYS_IOCTL,
            fd as usize,
            FBIOGET_VSCREENINFO,
            info.as_mut_ptr() as usize,
        );
        if r == 0 {
            let w = u32::from_le_bytes([info[0], info[1], info[2], info[3]]) as usize;
            let h = u32::from_le_bytes([info[4], info[5], info[6], info[7]]) as usize;
            let stride = u32::from_le_bytes([info[20], info[21], info[22], info[23]]) as usize;
            if w > 0 && h > 0 {
                FB_W = w;
                FB_H = h;
                FB_STRIDE_PX = if stride >= w * 4 { stride / 4 } else { w };
            }
        }
        let len = FB_W * FB_H * 4;
        let base = syscall6(
            SYS_MMAP,
            0,
            len,
            PROT_READ | PROT_WRITE,
            MAP_SHARED,
            fd as usize,
            0,
        );
        if (base as isize) < 0 {
            syscall1(SYS_CLOSE, fd as usize);
            return false;
        }
        FB_PTR = base as *mut u32;
        // 保留 fd：FBIO_PRESENT / FBIO_PUT_DAMAGE 依赖它提交脏区（mmap 后
        // 页表已指向画布，fd 仅用于 ioctl）。
        FB_FD = fd as isize;
        true
    }
}

/// 把 back buffer 的变更矩形拷贝到显存并提交呈现（脏区域直绘路径）。
///
/// 未显式调用 `mark_dirty` 时退化为整屏拷贝 + 整屏脏区（与旧行为一致）；
/// 调用过则只拷贝/提交已标记的矩形 —— 典型局部更新（时钟/光标/通知）从
/// 2MB 整屏 memcpy 降到矩形级拷贝。
pub fn flip() {
    unsafe {
        if FB_PTR.is_null() {
            return;
        }
        if DIRTY_COUNT == 0 && !DIRTY_FULL {
            DIRTY_FULL = true;
        }
        if DIRTY_FULL {
            // 整屏（含 stride 兼容的逐行回退）
            if FB_W == DISPLAY_W && FB_H == DISPLAY_H && FB_STRIDE_PX == DISPLAY_W {
                core::ptr::copy_nonoverlapping(BACK_BUF.as_ptr(), FB_PTR, DISPLAY_W * DISPLAY_H);
            } else {
                let w = FB_W.min(DISPLAY_W);
                let h = FB_H.min(DISPLAY_H);
                for y in 0..h {
                    core::ptr::copy_nonoverlapping(
                        BACK_BUF.as_ptr().add(y * DISPLAY_W),
                        FB_PTR.add(y * FB_STRIDE_PX),
                        w,
                    );
                }
            }
        } else {
            for i in 0..DIRTY_COUNT {
                copy_dirty_rect(DIRTY[i]);
            }
        }
        fb_submit_present();
    }
}

/// 提交脏区但不拷贝（如光标 overlay 已直接写入显存，仅需让内核合成+翻转）。
pub fn fb_damage_present() {
    fb_submit_present();
}

// ==================== 绘制原语 ====================

pub fn put_pixel(x: i32, y: i32, color: u32) {
    if x < 0 || y < 0 || x >= DISPLAY_W as i32 || y >= DISPLAY_H as i32 {
        return;
    }
    unsafe {
        BACK_BUF[y as usize * DISPLAY_W + x as usize] = color;
    }
}

pub fn fill_rect(x: i32, y: i32, w: i32, h: i32, color: u32) {
    if w <= 0 || h <= 0 {
        return;
    }
    // 先整体裁剪再逐行 slice::fill（编译为宽通道 memset）：导航栏/分隔条/
    // 按钮底等每帧万级 put_pixel 的逐像素边界检查降为每行一次。
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + w).min(DISPLAY_W as i32);
    let y1 = (y + h).min(DISPLAY_H as i32);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    unsafe {
        for yy in y0..y1 {
            let base = yy as usize * DISPLAY_W + x0 as usize;
            BACK_BUF[base..base + (x1 - x0) as usize].fill(color);
        }
    }
}

pub fn draw_rect(x: i32, y: i32, w: i32, h: i32, color: u32) {
    for dx in 0..w {
        put_pixel(x + dx, y, color);
        put_pixel(x + dx, y + h - 1, color);
    }
    for dy in 0..h {
        put_pixel(x, y + dy, color);
        put_pixel(x + w - 1, y + dy, color);
    }
}

// ==================== 抗锯齿圆角（对标 ColorOS 15 圆滑度） ====================

/// 以 alpha（0..=255）把颜色混合进 BACK_BUF。
pub fn blend_put(x: i32, y: i32, color: u32, alpha: u32) {
    if x < 0 || y < 0 || x >= DISPLAY_W as i32 || y >= DISPLAY_H as i32 {
        return;
    }
    unsafe {
        let idx = y as usize * DISPLAY_W + x as usize;
        BACK_BUF[idx] = nav::glass::blend(BACK_BUF[idx], color, alpha);
    }
}

/// BACK_BUF 抗锯齿绘制目标。
pub struct BufSmooth;

impl nav::glass::SmoothTarget for BufSmooth {
    fn put_aa(&mut self, x: i32, y: i32, color: u32, alpha: u32) {
        blend_put(x, y, color, alpha);
    }
}

/// 抗锯齿超椭圆圆角矩形填充（连续曲率，ColorOS 15 观感）。
pub fn aa_fill_round_rect(x: i32, y: i32, w: i32, h: i32, r: i32, color: u32) {
    nav::glass::smooth_fill_rr(&mut BufSmooth, x, y, w, h, r, color);
}

/// 抗锯齿圆角矩形 1px 描边。
pub fn aa_stroke_round_rect(x: i32, y: i32, w: i32, h: i32, r: i32, color: u32, alpha: u32) {
    nav::glass::smooth_stroke_rr(&mut BufSmooth, x, y, w, h, r, color, alpha);
}

/// 抗锯齿实心圆。
pub fn aa_fill_circle(cx: i32, cy: i32, r: i32, color: u32) {
    nav::glass::smooth_fill_circle(&mut BufSmooth, cx, cy, r, color);
}

/// Bresenham 直线。
pub fn draw_line(x0: i32, y0: i32, x1: i32, y1: i32, color: u32) {
    let mut x0 = x0;
    let mut y0 = y0;
    let dx = (x1 - x0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let dy = -(y1 - y0).abs();
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        put_pixel(x0, y0, color);
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x0 += sx;
        }
        if e2 <= dx {
            err += dx;
            y0 += sy;
        }
    }
}

/// 空心圆（中点画圆）。
/// 空心圆（抗锯齿 1px 圆环）。
pub fn draw_circle(cx: i32, cy: i32, r: i32, color: u32) {
    nav::glass::smooth_stroke_circle(&mut BufSmooth, cx, cy, r, color);
}

/// 实心圆（抗锯齿：圆周环带超采样混合）。
pub fn draw_disc(cx: i32, cy: i32, r: i32, color: u32) {
    aa_fill_circle(cx, cy, r, color);
}

// ==================== 8x8 位图字体（公有领域 IBM VGA 风格） ====================

pub static FONT8X8: [[u8; 8]; 95] = [
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // ' '
    [0x18, 0x3C, 0x3C, 0x18, 0x18, 0x00, 0x18, 0x00], // !
    [0x36, 0x36, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // "
    [0x36, 0x36, 0x7F, 0x36, 0x7F, 0x36, 0x36, 0x00], // #
    [0x0C, 0x3E, 0x03, 0x1E, 0x30, 0x1F, 0x0C, 0x00], // $
    [0x00, 0x63, 0x33, 0x18, 0x0C, 0x66, 0x63, 0x00], // %
    [0x1C, 0x36, 0x1C, 0x6E, 0x3B, 0x33, 0x6E, 0x00], // &
    [0x06, 0x06, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00], // '
    [0x18, 0x0C, 0x06, 0x06, 0x06, 0x0C, 0x18, 0x00], // (
    [0x06, 0x0C, 0x18, 0x18, 0x18, 0x0C, 0x06, 0x00], // )
    [0x00, 0x66, 0x3C, 0xFF, 0x3C, 0x66, 0x00, 0x00], // *
    [0x00, 0x0C, 0x0C, 0x3F, 0x0C, 0x0C, 0x00, 0x00], // +
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C, 0x06], // ,
    [0x00, 0x00, 0x00, 0x3F, 0x00, 0x00, 0x00, 0x00], // -
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C, 0x00], // .
    [0x60, 0x30, 0x18, 0x0C, 0x06, 0x03, 0x01, 0x00], // /
    [0x3E, 0x63, 0x73, 0x7B, 0x6F, 0x67, 0x3E, 0x00], // 0
    [0x0C, 0x0E, 0x0C, 0x0C, 0x0C, 0x0C, 0x3F, 0x00], // 1
    [0x1E, 0x33, 0x30, 0x1C, 0x06, 0x33, 0x3F, 0x00], // 2
    [0x1E, 0x33, 0x30, 0x1C, 0x30, 0x33, 0x1E, 0x00], // 3
    [0x38, 0x3C, 0x36, 0x33, 0x7F, 0x30, 0x78, 0x00], // 4
    [0x3F, 0x03, 0x1F, 0x30, 0x30, 0x33, 0x1E, 0x00], // 5
    [0x1C, 0x06, 0x03, 0x1F, 0x33, 0x33, 0x1E, 0x00], // 6
    [0x3F, 0x33, 0x30, 0x18, 0x0C, 0x0C, 0x0C, 0x00], // 7
    [0x1E, 0x33, 0x33, 0x1E, 0x33, 0x33, 0x1E, 0x00], // 8
    [0x1E, 0x33, 0x33, 0x3E, 0x30, 0x18, 0x0E, 0x00], // 9
    [0x00, 0x0C, 0x0C, 0x00, 0x00, 0x0C, 0x0C, 0x00], // :
    [0x00, 0x0C, 0x0C, 0x00, 0x00, 0x0C, 0x0C, 0x06], // ;
    [0x18, 0x0C, 0x06, 0x03, 0x06, 0x0C, 0x18, 0x00], // <
    [0x00, 0x00, 0x3F, 0x00, 0x00, 0x3F, 0x00, 0x00], // =
    [0x06, 0x0C, 0x18, 0x30, 0x18, 0x0C, 0x06, 0x00], // >
    [0x1E, 0x33, 0x30, 0x18, 0x0C, 0x00, 0x0C, 0x00], // ?
    [0x3E, 0x63, 0x7B, 0x7B, 0x7B, 0x03, 0x1E, 0x00], // @
    [0x0C, 0x1E, 0x33, 0x33, 0x3F, 0x33, 0x33, 0x00], // A
    [0x3F, 0x66, 0x66, 0x3E, 0x66, 0x66, 0x3F, 0x00], // B
    [0x3C, 0x66, 0x03, 0x03, 0x03, 0x66, 0x3C, 0x00], // C
    [0x1F, 0x36, 0x66, 0x66, 0x66, 0x36, 0x1F, 0x00], // D
    [0x7F, 0x46, 0x16, 0x1E, 0x16, 0x46, 0x7F, 0x00], // E
    [0x7F, 0x46, 0x16, 0x1E, 0x16, 0x06, 0x0F, 0x00], // F
    [0x3C, 0x66, 0x03, 0x03, 0x73, 0x66, 0x7C, 0x00], // G
    [0x33, 0x33, 0x33, 0x3F, 0x33, 0x33, 0x33, 0x00], // H
    [0x1E, 0x0C, 0x0C, 0x0C, 0x0C, 0x0C, 0x1E, 0x00], // I
    [0x78, 0x30, 0x30, 0x30, 0x33, 0x33, 0x1E, 0x00], // J
    [0x67, 0x66, 0x36, 0x1E, 0x36, 0x66, 0x67, 0x00], // K
    [0x0F, 0x06, 0x06, 0x06, 0x46, 0x66, 0x7F, 0x00], // L
    [0x63, 0x77, 0x7F, 0x7F, 0x6B, 0x63, 0x63, 0x00], // M
    [0x63, 0x67, 0x6F, 0x7B, 0x73, 0x63, 0x63, 0x00], // N
    [0x1C, 0x36, 0x63, 0x63, 0x63, 0x36, 0x1C, 0x00], // O
    [0x3F, 0x66, 0x66, 0x3E, 0x06, 0x06, 0x0F, 0x00], // P
    [0x1E, 0x33, 0x33, 0x33, 0x3B, 0x1E, 0x38, 0x00], // Q
    [0x3F, 0x66, 0x66, 0x3E, 0x36, 0x66, 0x67, 0x00], // R
    [0x1E, 0x33, 0x07, 0x0E, 0x38, 0x33, 0x1E, 0x00], // S
    [0x3F, 0x2D, 0x0C, 0x0C, 0x0C, 0x0C, 0x1E, 0x00], // T
    [0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x3F, 0x00], // U
    [0x33, 0x33, 0x33, 0x33, 0x33, 0x1E, 0x0C, 0x00], // V
    [0x63, 0x63, 0x63, 0x6B, 0x7F, 0x77, 0x63, 0x00], // W
    [0x63, 0x63, 0x36, 0x1C, 0x1C, 0x36, 0x63, 0x00], // X
    [0x33, 0x33, 0x33, 0x1E, 0x0C, 0x0C, 0x1E, 0x00], // Y
    [0x7F, 0x63, 0x31, 0x18, 0x4C, 0x66, 0x7F, 0x00], // Z
    [0x1E, 0x06, 0x06, 0x06, 0x06, 0x06, 0x1E, 0x00], // [
    [0x03, 0x06, 0x0C, 0x18, 0x30, 0x60, 0x40, 0x00], // 反斜杠
    [0x1E, 0x18, 0x18, 0x18, 0x18, 0x18, 0x1E, 0x00], // ]
    [0x08, 0x1C, 0x36, 0x63, 0x00, 0x00, 0x00, 0x00], // ^
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF], // _
    [0x0C, 0x0C, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00], // `
    [0x00, 0x00, 0x1E, 0x30, 0x3E, 0x33, 0x6E, 0x00], // a
    [0x07, 0x06, 0x06, 0x3E, 0x66, 0x66, 0x3B, 0x00], // b
    [0x00, 0x00, 0x1E, 0x33, 0x03, 0x33, 0x1E, 0x00], // c
    [0x38, 0x30, 0x30, 0x3E, 0x33, 0x33, 0x6E, 0x00], // d
    [0x00, 0x00, 0x1E, 0x33, 0x3F, 0x03, 0x1E, 0x00], // e
    [0x1C, 0x36, 0x06, 0x0F, 0x06, 0x06, 0x0F, 0x00], // f
    [0x00, 0x00, 0x6E, 0x33, 0x33, 0x3E, 0x30, 0x1F], // g
    [0x07, 0x06, 0x36, 0x6E, 0x66, 0x66, 0x67, 0x00], // h
    [0x0C, 0x00, 0x0E, 0x0C, 0x0C, 0x0C, 0x1E, 0x00], // i
    [0x30, 0x00, 0x30, 0x30, 0x30, 0x33, 0x33, 0x1E], // j
    [0x07, 0x06, 0x66, 0x36, 0x1E, 0x36, 0x67, 0x00], // k
    [0x0E, 0x0C, 0x0C, 0x0C, 0x0C, 0x0C, 0x1E, 0x00], // l
    [0x00, 0x00, 0x33, 0x7F, 0x7F, 0x6B, 0x63, 0x00], // m
    [0x00, 0x00, 0x1F, 0x33, 0x33, 0x33, 0x33, 0x00], // n
    [0x00, 0x00, 0x1E, 0x33, 0x33, 0x33, 0x1E, 0x00], // o
    [0x00, 0x00, 0x3B, 0x66, 0x66, 0x3E, 0x06, 0x0F], // p
    [0x00, 0x00, 0x6E, 0x33, 0x33, 0x3E, 0x30, 0x78], // q
    [0x00, 0x00, 0x3B, 0x6E, 0x66, 0x06, 0x0F, 0x00], // r
    [0x00, 0x00, 0x3E, 0x03, 0x1E, 0x30, 0x1F, 0x00], // s
    [0x08, 0x0C, 0x3E, 0x0C, 0x0C, 0x2C, 0x18, 0x00], // t
    [0x00, 0x00, 0x33, 0x33, 0x33, 0x33, 0x6E, 0x00], // u
    [0x00, 0x00, 0x33, 0x33, 0x33, 0x1E, 0x0C, 0x00], // v
    [0x00, 0x00, 0x63, 0x6B, 0x7F, 0x7F, 0x36, 0x00], // w
    [0x00, 0x00, 0x63, 0x36, 0x1C, 0x36, 0x63, 0x00], // x
    [0x00, 0x00, 0x33, 0x33, 0x33, 0x3E, 0x30, 0x1F], // y
    [0x00, 0x00, 0x3F, 0x19, 0x0C, 0x26, 0x3F, 0x00], // z
    [0x38, 0x0C, 0x0C, 0x07, 0x0C, 0x0C, 0x38, 0x00], // {
    [0x18, 0x18, 0x18, 0x00, 0x18, 0x18, 0x18, 0x00], // |
    [0x07, 0x0C, 0x0C, 0x38, 0x0C, 0x0C, 0x07, 0x00], // }
    [0x6E, 0x3B, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // ~
];

/// 取 ASCII 字形（0x20..=0x7E；其余返回空格）。兼容旧调用方；文本渲染已走 TTF。
pub fn glyph(c: u8) -> &'static [u8; 8] {
    if c >= 0x20 && c <= 0x7E {
        &FONT8X8[(c - 0x20) as usize]
    } else {
        &FONT8X8[0]
    }
}

/// 字形位图混合绘制（三个 TTF 字形路径共用）：与屏幕求交一次后直写
/// BACK_BUF —— 此前每像素 4 次边界比较 + put_pixel 再次检查；a==255 直通。
fn blit_glyph(data: &[u8], gw: usize, gh: usize, gx: i32, gy: i32, color: u32) {
    let x0 = gx.max(0);
    let y0 = gy.max(0);
    let x1 = (gx + gw as i32).min(DISPLAY_W as i32);
    let y1 = (gy + gh as i32).min(DISPLAY_H as i32);
    if x1 <= x0 || y1 <= y0 || gw == 0 {
        return;
    }
    let col0 = (x0 - gx) as usize;
    let col1 = (x1 - gx) as usize;
    let row0 = (y0 - gy) as usize;
    let row1 = (y1 - gy) as usize;
    unsafe {
        for row in row0..row1 {
            let src = &data[row * gw + col0..row * gw + col1];
            let base = (y0 + (row - row0) as i32) as usize * DISPLAY_W + x0 as usize;
            for (ci, &a) in src.iter().enumerate() {
                if a == 0 {
                    continue;
                }
                let idx = base + ci;
                BACK_BUF[idx] = if a >= 255 {
                    color
                } else {
                    ttf_text::font_render::blend(BACK_BUF[idx], color, a)
                };
            }
        }
    }
}

/// 把单个码点以 cn.ttf 平滑字形绘制到 `(x, y)`（y 为字形顶部），返回推进宽度。
/// 字体未就绪时退回固定宽度（CJK 16 / ASCII 8）且不绘制。
pub fn draw_glyph_ttf(x: i32, y: i32, cp: u32, color: u32) -> i32 {
    if !ttf_text::font_render::font_ready() {
        return if cp >= 0x80 { 16 } else { 8 };
    }
    let px = ttf_text::font_render::FONT_PX;
    let ch = core::char::from_u32(cp).unwrap_or(' ');
    if let Some(g) = ttf_text::font_render::rasterize_char(ch, px) {
        let gx = x + g.xmin;
        let gy = ttf_text::font_render::glyph_draw_top(y, g.ymin, g.height, px);
        // 背景取自当前 back buffer，而非上一帧的显存 FB_PTR，
        // 否则混合进旧帧内容会出现文字残影/错位（鬼画符）。
        blit_glyph(&g.data, g.width, g.height, gx, gy, color);
        return g.advance;
    }
    0
}

/// 把单个码点以 bold.ttf 标题字形绘制到 `(x, y)`，返回推进宽度。
/// 标题字体未就绪或缺少该字形时退回常规字体。
pub fn draw_glyph_bold_ttf(x: i32, y: i32, cp: u32, color: u32) -> i32 {
    if !ttf_text::font_render::font_bold_ready() || !ttf_text::font_render::has_glyph_bold(cp) {
        return draw_glyph_ttf(x, y, cp, color);
    }
    let px = ttf_text::font_render::FONT_PX;
    let ch = core::char::from_u32(cp).unwrap_or(' ');
    if let Some(g) = ttf_text::font_render::rasterize_char_bold(ch, px) {
        let gx = x + g.xmin;
        let gy = ttf_text::font_render::glyph_draw_top_bold(y, g.ymin, g.height, px);
        blit_glyph(&g.data, g.width, g.height, gx, gy, color);
        return g.advance;
    }
    0
}

/// 把 Phosphor 图标（码点见 `ttf_text::font_render::icons`）栅格化并居中绘制：
/// `(cx, cy)` 为中心，字形位图（墨迹）居中于该点，`px` 为 em 字号。
/// `fill=true` 用 Phosphor-Fill 实心，false 用 Phosphor-Bold 粗线。
/// 返回实际墨迹宽度；图标字体未就绪或缺字形返回 0（调用方可退回矢量绘制）。
pub fn draw_icon_centered(fill: bool, cx: i32, cy: i32, cp: u32, px: f32, color: u32) -> i32 {
    let Some(g) = ttf_text::font_render::rasterize_icon(fill, cp, px) else {
        return 0;
    };
    if g.width == 0 || g.height == 0 {
        return 0;
    }
    let x0 = cx - g.width as i32 / 2;
    let y0 = cy - g.height as i32 / 2;
    // 背景取自当前 back buffer（与 draw_glyph_ttf 同理，避免残影）。
    blit_glyph(&g.data, g.width, g.height, x0, y0, color);
    g.width as i32
}

/// 以 bold.ttf 标题字体按 UTF-8 渲染文本（标题使用）。
pub fn draw_text_bold(x: i32, y: i32, s: &[u8], color: u32) {    let mut cx = x;
    let mut i = 0;
    while i < s.len() {
        let (cp, n) = ttf_text::font_render::utf8_next(s, i);
        cx += draw_glyph_bold_ttf(cx, y, cp, color);
        i += n;
    }
}

/// 标题文本宽度（像素，bold.ttf 字体度量）。
pub fn text_width_bold(s: &[u8]) -> i32 {
    ttf_text::font_render::text_width_bold(s, ttf_text::font_render::FONT_PX)
}

/// 水平居中绘制标题（bold.ttf）。
pub fn draw_center_bold(y: i32, s: &[u8], color: u32) {
    let tx = (DISPLAY_W as i32 - text_width_bold(s)) / 2;
    draw_text_bold(tx, y, s, color);
}

/// 绘制单个字符（cn.ttf 平滑渲染）。
pub fn draw_char(x: i32, y: i32, c: u8, color: u32) {
    draw_glyph_ttf(x, y, c as u32, color);
}

/// 绘制单个 CJK 码点（cn.ttf 平滑渲染；参数为 Unicode 码点，不再是点阵索引）。
pub fn draw_cjk(x: i32, y: i32, cp: u16, color: u32) {
    draw_glyph_ttf(x, y, cp as u32, color);
}

/// 按 UTF-8 渲染文本（cn.ttf 平滑字体）。
pub fn draw_text(x: i32, y: i32, s: &[u8], color: u32) {
    let mut cx = x;
    let mut i = 0;
    while i < s.len() {
        let (cp, n) = ttf_text::font_render::utf8_next(s, i);
        cx += draw_glyph_ttf(cx, y, cp, color);
        i += n;
    }
}

pub fn text_width(s: &[u8]) -> i32 {
    ttf_text::font_render::text_width(s, ttf_text::font_render::FONT_PX)
}

/// 水平居中绘制。
pub fn draw_center(y: i32, s: &[u8], color: u32) {
    let tx = (DISPLAY_W as i32 - text_width(s)) / 2;
    draw_text(tx, y, s, color);
}

// ==================== 错误提示横幅 ====================

/// 错误横幅配色：暗红底 + 亮红描边 + 白字。
pub const C_ERR_BG: u32 = 0x00_7A_1F_1F;
pub const C_ERR_BORDER: u32 = 0x00_C0_40_40;
pub const ERR_BAR_H: i32 = 44;
pub const ERR_BAR_Y: i32 = 48;

/// 若存在未过期的错误提示，在标题栏下方绘制横幅。
/// 应放在应用内容之后、`draw_sys_nav` 之前调用，保证横幅在最上层。
/// 采用红色液态玻璃材质。
pub fn draw_error_banner() {
    if !error_ui::err_active(now_us()) {
        return;
    }
    let msg = error_ui::err_msg();
    if msg.is_empty() {
        return;
    }
    draw_glass_style(0, ERR_BAR_Y, DISPLAY_W as i32, ERR_BAR_H, 0, &STYLE_DANGER);
    let tx = (DISPLAY_W as i32 - text_width(msg)) / 2;
    draw_text(tx, ERR_BAR_Y + 14, msg, 0x00_FF_FF_FF);
}

/// 点按错误横幅即清除（在 tap 处理中调用）。
pub fn error_banner_hit(x: i32, y: i32) -> bool {
    let hit = x >= 0 && x < DISPLAY_W as i32 && y >= ERR_BAR_Y && y < ERR_BAR_Y + ERR_BAR_H;
    if hit {
        error_ui::err_clear();
    }
    hit
}

// ==================== 标题栏与返回键 ====================

/// 绘制标题栏：液态玻璃底（背景模糊 + 浅色着色 + 底部高光描边）+
/// 左上角玻璃返回键 + 居中标题（bold.ttf 标题字体）。
pub fn draw_title_bar(title: &[u8]) {
    draw_glass(0, 0, DISPLAY_W as i32, TITLE_H, 0);
    draw_glass(BACK_X, BACK_Y, BACK_SZ - 8, BACK_SZ - 8, 10);
    draw_text(BACK_X + 10, BACK_Y + 10, b"<", C_TEXT);
    draw_center_bold((TITLE_H - 8) / 2, title, C_TEXT);
}

pub fn back_hit(x: i32, y: i32) -> bool {
    x >= BACK_X && x < BACK_X + BACK_SZ && y >= BACK_Y && y < BACK_Y + BACK_SZ
}

// ==================== 系统导航栏 ====================

/// 内容区底部（导航栏之上），应用布局可据此收缩高度避免被导航栏遮挡。
pub fn content_bottom() -> i32 {
    NAV_Y - 8
}

/// 导航栏点击命中（仅 y >= NAV_Y 时可能命中）。
pub fn sys_nav_hit(x: i32, y: i32) -> NavAction {
    if y >= NAV_Y {
        nav::nav_hit(x, y, nav::nav_mode())
    } else {
        NavAction::None
    }
}

/// 把导航动作映射为进程退出码（launcher 据此决定回到桌面后的状态）。
pub fn nav_exit_code(a: NavAction) -> usize {
    nav::nav_exit_code(a)
}

/// 把系统导航栏绘制进 back buffer（三键或手势指示条，按当前模式）。
/// 所有 app_kit 应用的 render 末尾都会调用本函数，因此同时把未过期的
/// 错误提示横幅画在最上层（应用内容之后、导航栏之前）。
pub fn draw_sys_nav() {
    draw_error_banner();
    struct BackBufCanvas;
    impl NavCanvas for BackBufCanvas {
        fn put(&mut self, x: i32, y: i32, color: u32) {
            put_pixel(x, y, color);
        }
        fn backdrop(&self, x: i32, y: i32) -> u32 {
            if x < 0 || y < 0 || x >= DISPLAY_W as i32 || y >= DISPLAY_H as i32 {
                0
            } else {
                unsafe { BACK_BUF[y as usize * DISPLAY_W + x as usize] }
            }
        }
    }
    let mut cv = BackBufCanvas;
    nav::draw_nav_bar(&mut cv, nav::nav_mode());
}

// ==================== 光标 ====================
//
// 指针贴图（tubiao/shubiao.png 生成，24x24 + 热点）见 cursor_sprite 模块。

#[path = "cursor_sprite.rs"]
mod cursor_sprite;

/// 把光标画进 back buffer（供整屏 render 使用）。
pub fn draw_cursor() {
    unsafe {
        cursor_sprite::draw_at(
            BACK_BUF.as_mut_ptr(),
            DISPLAY_W,
            DISPLAY_W as i32,
            DISPLAY_H as i32,
            CUR_X,
            CUR_Y,
            None,
        );
    }
}

// ==================== 光标 overlay 快速路径 ====================
//
// 鼠标移动只更新显存 FB 上光标所在的 26x26 小区域（恢复旧位置 → 备份新
// 位置 → 画光标），不再整屏 render()+flip()。此前每个移动事件都触发整屏
// 重绘（TTF 文字重新栅格化 + 2MB flip），QEMU 软件渲染下光标严重不跟手。
// 各应用 render() 不再把光标画进 BACK_BUF；整屏重绘后调用
// `cursor_overlay_redraw()`，移动时调用 `cursor_overlay_move()`。

const CUR_UW: usize = cursor_sprite::CUR_UW as usize;
const CUR_UH: usize = cursor_sprite::CUR_UH as usize;

/// 光标下 26x26 区域的备份（贴图 24x24 + 1px 边距），用于 overlay 移动时擦除旧光标。
static mut CUR_UNDER: [u32; CUR_UW * CUR_UH] = [0u32; CUR_UW * CUR_UH];
static mut CUR_UNDER_X: i32 = -1;
static mut CUR_UNDER_Y: i32 = -1;

fn fb_px(x: i32, y: i32) -> u32 {
    unsafe {
        if x < 0 || y < 0 || x >= FB_W as i32 || y >= FB_H as i32 {
            0
        } else {
            *FB_PTR.add(y as usize * FB_STRIDE_PX + x as usize)
        }
    }
}

fn fb_set_px(x: i32, y: i32, color: u32) {
    unsafe {
        if x >= 0 && y >= 0 && x < FB_W as i32 && y < FB_H as i32 {
            *FB_PTR.add(y as usize * FB_STRIDE_PX + x as usize) = color;
        }
    }
}

fn save_cursor_under(x: i32, y: i32) {
    unsafe {
        let sx = cursor_sprite::under_x(x);
        let sy = cursor_sprite::under_y(y);
        CUR_UNDER_X = sx;
        CUR_UNDER_Y = sy;
        for dy in 0..CUR_UH as i32 {
            for dx in 0..CUR_UW as i32 {
                CUR_UNDER[(dy * CUR_UW as i32 + dx) as usize] = fb_px(sx + dx, sy + dy);
            }
        }
    }
}

fn restore_cursor_under() {
    unsafe {
        let x = CUR_UNDER_X;
        let y = CUR_UNDER_Y;
        for dy in 0..CUR_UH as i32 {
            for dx in 0..CUR_UW as i32 {
                fb_set_px(
                    x + dx,
                    y + dy,
                    CUR_UNDER[(dy * CUR_UW as i32 + dx) as usize],
                );
            }
        }
    }
}

fn draw_cursor_fb() {
    unsafe {
        cursor_sprite::draw_at(
            FB_PTR,
            FB_STRIDE_PX,
            FB_W as i32,
            FB_H as i32,
            CUR_X,
            CUR_Y,
            None,
        );
    }
}

/// 提交任意小脏矩形并请求呈现（FBIO_PRESENT 快速路径）。
///
/// 光标移动等局部更新专用：一次 ioctl 只携带 ~KB 级矩形列表，内核只把这些
/// 矩形从画布拷入 back 并翻转，替代整屏 2MB 提交（QEMU 软件渲染下整屏
/// 提交在屏蔽中断的 ioctl 内执行，是系统级卡顿的直接原因）。
pub fn fb_present_rects(rects: &[(i32, i32, u32, u32)]) {
    unsafe {
        if FB_FD < 0 || FB_PTR.is_null() || rects.is_empty() {
            return;
        }
        let n = rects.len().min(MAX_DIRTY);
        DAMAGE_BUF[0..4].copy_from_slice(&(n as u32).to_le_bytes());
        for i in 0..n {
            let r = rects[i];
            let off = 4 + i * 16;
            DAMAGE_BUF[off..off + 4].copy_from_slice(&r.0.to_le_bytes());
            DAMAGE_BUF[off + 4..off + 8].copy_from_slice(&r.1.to_le_bytes());
            DAMAGE_BUF[off + 8..off + 12].copy_from_slice(&r.2.to_le_bytes());
            DAMAGE_BUF[off + 12..off + 16].copy_from_slice(&r.3.to_le_bytes());
        }
        syscall3(SYS_IOCTL, FB_FD as usize, FBIO_PRESENT, DAMAGE_BUF.as_ptr() as usize);
    }
}

/// 整屏重绘（render + flip）后调用：备份光标下像素并以 overlay 画回显存，
/// 随后把光标区域作为小脏矩形提交呈现（flip 的整屏提交不含光标像素，
/// 不补这次提交光标不会出现在扫描输出上）。
pub fn cursor_overlay_redraw() {
    unsafe {
        if !FB_PTR.is_null() {
            save_cursor_under(CUR_X, CUR_Y);
            draw_cursor_fb();
            fb_present_rects(&[(
                CUR_UNDER_X,
                CUR_UNDER_Y,
                CUR_UW as u32,
                CUR_UH as u32,
            )]);
        }
    }
}

/// 光标移动快速路径：只更新 FB 上光标所在小区域，并立即以小脏矩形提交呈现。
///
/// 提交 `[旧位置区, 新位置区]` 两个 ~26x26 矩形，内核脏区拷贝 + 翻转都在
/// 微秒级完成 —— 输入→画面延迟从「整屏提交数 ms + 等下一次全量重绘」降到
/// 单次小矩形呈现。
pub fn cursor_overlay_move() {
    unsafe {
        if FB_PTR.is_null() {
            return;
        }
        let old = if CUR_UNDER_X >= 0 {
            (CUR_UNDER_X, CUR_UNDER_Y, CUR_UW as u32, CUR_UH as u32)
        } else {
            (0, 0, 0, 0)
        };
        restore_cursor_under();
        save_cursor_under(CUR_X, CUR_Y);
        draw_cursor_fb();
        let new = (CUR_UNDER_X, CUR_UNDER_Y, CUR_UW as u32, CUR_UH as u32);
        fb_present_rects(&[old, new]);
    }
}

// ==================== 输入 ====================

/// 单次输入轮询结果。
pub struct PollResult {
    /// 本次抬起产生的点击坐标（系统导航已消费的点击不在此列）。
    pub tap: Option<(i32, i32)>,
    /// 指针是否移动（需要重绘）。
    pub moved: bool,
    /// 键盘动作（最后一次）。
    pub key: KeyAction,
    /// 系统导航动作（返回/桌面/最近任务；由导航栏点击或手势产生）。
    pub nav: NavAction,
}

#[derive(Clone, Copy, PartialEq)]
pub enum KeyAction {
    None,
    Char(u8),
    Backspace,
    Enter,
    Esc,
}

/// evdev 键码 → ASCII 字符（QWERTY 物理布局逐键映射，与 oobe/terminal 同表）。
pub fn key_to_char(code: u16, shift: bool) -> Option<u8> {
    let c = match code {
        2 => b'1',
        3 => b'2',
        4 => b'3',
        5 => b'4',
        6 => b'5',
        7 => b'6',
        8 => b'7',
        9 => b'8',
        10 => b'9',
        11 => b'0',
        16 => b'q',
        17 => b'w',
        18 => b'e',
        19 => b'r',
        20 => b't',
        21 => b'y',
        22 => b'u',
        23 => b'i',
        24 => b'o',
        25 => b'p',
        30 => b'a',
        31 => b's',
        32 => b'd',
        33 => b'f',
        34 => b'g',
        35 => b'h',
        36 => b'j',
        37 => b'k',
        38 => b'l',
        44 => b'z',
        45 => b'x',
        46 => b'c',
        47 => b'v',
        48 => b'b',
        49 => b'n',
        50 => b'm',
        57 => b' ',
        12 => b'-',
        13 => b'=',
        52 => b'.',
        51 => b',',
        53 => b'/',
        39 => b';',
        77 => b'@',
        _ => return None,
    };
    let c = if shift && c.is_ascii_lowercase() {
        c - 32
    } else {
        c
    };
    if shift {
        Some(match c {
            b'1' => b'!',
            b'2' => b'@',
            b'3' => b'#',
            b'4' => b'$',
            b'5' => b'%',
            b'6' => b'^',
            b'7' => b'&',
            b'8' => b'*',
            b'9' => b'(',
            b'0' => b')',
            b'-' => b'_',
            b'=' => b'+',
            b';' => b':',
            b'.' => b'>',
            b',' => b'<',
            b'/' => b'?',
            _ => c,
        })
    } else {
        Some(c)
    }
}

/// 轮询 /dev/input/event0：指针移动、点击抬起、键盘按键。
///
/// 每帧先分发生命周期事件（`lc_poll`）：后台化期间的 PAUSE/SUSPEND、
/// 恢复后的 RESUME（默认回调置重绘标记）、推送 NOTIFY 与 LMK TERMINATE
/// （默认动作：保存后退出）。应用无需为基本后台/恢复做任何额外处理。
pub fn poll_input() -> PollResult {
    unsafe {
        // 生命周期事件优先于输入分发（TERMINATE 会退出；RESUME 置重绘标记）。
        lifecycle::lc_poll();
        // 光随指动缓动推进（触摸中 120ms FastOutSlowIn 跟随；离开后回零）。
        if glass::light_frame(now_us()) {
            LIGHT_ANIM = true;
        }
        if INPUT_FD < 0 {
            INPUT_FD = open(INPUT_PATH, O_RDONLY | O_NONBLOCK);
        }
        let mut res = PollResult {
            tap: None,
            moved: false,
            key: KeyAction::None,
            nav: NavAction::None,
        };
        if INPUT_FD < 0 {
            // 输入设备不可用：持续提示（每次轮询刷新计时，保持横幅可见）。
            INPUT_ERR_SHOWN = true;
            error_ui::err_set("输入设备不可用".as_bytes(), now_us());
            return res;
        }
        // 输入恢复后清除此前因设备缺失设置的提示。
        if INPUT_ERR_SHOWN {
            INPUT_ERR_SHOWN = false;
            error_ui::err_clear();
        }
        loop {
            let mut buf = [0u8; 24];
            let n = syscall3(SYS_READ, INPUT_FD as usize, buf.as_mut_ptr() as usize, 24);
            if (n as isize) <= 0 || n < 24 {
                return res;
            }
            let etype = u16::from_le_bytes([buf[16], buf[17]]);
            let code = u16::from_le_bytes([buf[18], buf[19]]);
            let value = i32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]);
            match etype {
                EV_REL => {
                    if code == REL_X {
                        CUR_X = (CUR_X + value).clamp(0, DISPLAY_W as i32 - 1);
                        res.moved = true;
                    } else if code == REL_Y {
                        CUR_Y = (CUR_Y + value).clamp(0, DISPLAY_H as i32 - 1);
                        res.moved = true;
                    }
                }
                EV_ABS => {
                    if code == ABS_X {
                        CUR_X = value.clamp(0, DISPLAY_W as i32 - 1);
                        res.moved = true;
                    } else if code == ABS_Y {
                        CUR_Y = value.clamp(0, DISPLAY_H as i32 - 1);
                        res.moved = true;
                    }
                    // 光随指动：触摸/触控笔拖动期间喂光源目标（P2-2）。
                    if TOUCH_DOWN && (code == ABS_X || code == ABS_Y) {
                        glass::light_track(
                            CUR_X,
                            CUR_Y,
                            DISPLAY_W as i32,
                            DISPLAY_H as i32,
                            now_us(),
                        );
                    }
                }
                EV_KEY => {
                    match code {
                        BTN_LEFT => {
                            // 鼠标左键（QEMU GUI 的 virtio-tablet 点击也以 BTN_LEFT 上报）：
                            // 与 BTN_TOUCH 共用同一套手势识别 —— 全面屏手势（底部上滑 /
                            // 边缘返回）在 QEMU 下同样可用。
                            if value == 1 {
                                MOUSE_DOWN = true;
                                TOUCH_DOWN = true;
                                TOUCH_PX = CUR_X;
                                TOUCH_PY = CUR_Y;
                                TOUCH_US = now_us();
                            } else if value == 0 && MOUSE_DOWN {
                                MOUSE_DOWN = false;
                                if TOUCH_DOWN {
                                    TOUCH_DOWN = false;
                                    let mode = nav::nav_mode();
                                    // 1) 导航栏区域点击（三键模式）
                                    let a = nav::nav_hit(CUR_X, CUR_Y, mode);
                                    if a != NavAction::None {
                                        res.nav = a;
                                    } else {
                                        // 2) 全面屏手势：底部上滑 / 边缘返回
                                        let dur = now_us().saturating_sub(TOUCH_US) / 1000;
                                        let g = nav::classify_swipe(
                                            TOUCH_PX, TOUCH_PY, CUR_X, CUR_Y, dur, mode,
                                        );
                                        if g != NavAction::None {
                                            res.nav = g;
                                        } else {
                                            res.tap = Some((CUR_X, CUR_Y));
                                        }
                                    }
                                }
                            }
                        }
                        BTN_TOUCH => {
                            if value == 1 {
                                TOUCH_DOWN = true;
                                TOUCH_PX = CUR_X;
                                TOUCH_PY = CUR_Y;
                                TOUCH_US = now_us();
                            } else if value == 0 && TOUCH_DOWN {
                                TOUCH_DOWN = false;
                                let mode = nav::nav_mode();
                                // 1) 导航栏区域点击（三键模式）
                                let a = nav::nav_hit(CUR_X, CUR_Y, mode);
                                if a != NavAction::None {
                                    res.nav = a;
                                } else {
                                    // 2) 全面屏手势：底部上滑 / 边缘返回
                                    let dur = now_us().saturating_sub(TOUCH_US) / 1000;
                                    let g = nav::classify_swipe(
                                        TOUCH_PX, TOUCH_PY, CUR_X, CUR_Y, dur, mode,
                                    );
                                    if g != NavAction::None {
                                        res.nav = g;
                                    } else {
                                        res.tap = Some((CUR_X, CUR_Y));
                                    }
                                }
                            }
                        }
                        KEY_LEFTSHIFT | KEY_RIGHTSHIFT => {
                            SHIFT = value == 1;
                        }
                        KEY_BACKSPACE => {
                            if value == 1 {
                                res.key = KeyAction::Backspace;
                            }
                        }
                        KEY_ENTER => {
                            if value == 1 {
                                res.key = KeyAction::Enter;
                            }
                        }
                        KEY_ESC => {
                            if value == 1 {
                                res.key = KeyAction::Esc;
                            }
                        }
                        _ => {
                            if value == 1 {
                                if let Some(c) = key_to_char(code, SHIFT) {
                                    res.key = KeyAction::Char(c);
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// 轮询 /dev/tty（fd 0）是否收到 ESC。
pub fn poll_tty_esc() -> bool {
    let mut buf = [0u8; 8];
    let n = unsafe { syscall3(SYS_READ, 0, buf.as_mut_ptr() as usize, buf.len()) } as isize;
    if n <= 0 {
        return false;
    }
    let m = (n as usize).min(buf.len());
    for i in 0..m {
        if buf[i] == 0x1b {
            return true;
        }
    }
    false
}
