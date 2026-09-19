// KunxOS 设置应用：全功能系统设置（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 对标 iOS「设置」/ Android「设置」的完整设置体验：
//   主页面 -> 分类子页 -> 具体设置项（开关 / 滑块 / 选择器 / 输入框 / 确认框），
//   顶部全局搜索入口，配置落盘 /data/settings/<分类>.conf（手写 key=value），
//   修改即时生效并发布到既有子系统消费端（/data/oobe/*、/var/run/wifid.pipe、
//   电源 syscall 411、/data/settings/notify 事件文件），服务端按文件监听即可感知。
//
// 页面模型：静态页栈（深度 <= 8）+ 每页滚动偏移；输入页/图案页/确认页共用
// 上下文标记。绘制复用毛玻璃/液态玻璃风格（同 launcher），触摸 + 鼠标滚轮 +
// 物理键盘全套输入在 /dev/input/event0 上驱动。
//
// 说明：
//  - 亮度/深色/颜色反转/高对比度/字体大小在本应用内即时生效（flip LUT 缩放），
//    全系统生效需各应用与合成器协作（预留）。
//  - Wi-Fi 经 /var/run/wifid.pipe 真实下发；锁屏信息写回 /data/oobe/*，
//    由 launcher 锁屏实时校验；低电量模式经 syscall 411 切 governor。
//  - 蓝牙/移动数据/VPN/NFC/存储统计等无后备子系统的保留接口，落盘 + 提示。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

#[path = "../../../shared/ttf_text.rs"]
mod ttf_text;

// 共享系统导航（导航方式切换 + 底部导航栏/手势）。
#[path = "../../../shared/nav.rs"]
mod sys_nav;

// 共享错误提示状态（横幅绘制见 draw_toast）。
#[path = "../../../shared/error_ui.rs"]
mod err_ui;

// 共享光标 overlay 快速路径（鼠标移动不整屏重绘）。
#[path = "../../../shared/cursor_overlay.rs"]
mod cursor_overlay;

// 共享时区数据库（日期与时间页）。
#[path = "../../../shared/tz.rs"]
mod sys_tz;

// 共享运动库（贝塞尔缓动 + 弹簧，docs/animations.md 应用侧实现）。
#[path = "../../../shared/motion.rs"]
mod motion;

// 内部模块。
mod cfg;
mod ui;
mod pages;
mod pages2;

use ui::*;
use cfg::*;
use pages::*;

// ==================== 系统调用号（AArch64 asm-generic） ====================

pub(crate) const SYS_READ: usize = 63;
pub(crate) const SYS_WRITE: usize = 64;
pub(crate) const SYS_OPENAT: usize = 56;
pub(crate) const SYS_CLOSE: usize = 57;
pub(crate) const SYS_IOCTL: usize = 29;
pub(crate) const SYS_EXIT: usize = 93;
pub(crate) const SYS_NANOSLEEP: usize = 101;
pub(crate) const SYS_MMAP: usize = 222;
pub(crate) const SYS_GETTIMEOFDAY: usize = 169;
pub(crate) const SYS_GETDENTS64: usize = 61;
pub(crate) const SYS_STATFS: usize = 43;
pub(crate) const SYS_MKDIRAT: usize = 34;
pub(crate) const SYS_UNLINKAT: usize = 35;
pub(crate) const SYS_KUNX_OTA: usize = 430;
pub(crate) const SYS_REBOOT: usize = 142;
pub(crate) const SYS_PM_GET: usize = 410;
pub(crate) const SYS_PM_GOVERNOR: usize = 411;

// 电源 governor id：0=ondemand 1=conservative 2=powersave 3=performance
pub(crate) const GOV_ONDEMAND: usize = 0;
pub(crate) const GOV_POWERSAVE: usize = 2;

// reboot(2) 魔数（kunxcore linux_compat::sys_reboot）。
pub(crate) const REBOOT_CMD: usize = 0x0123_4567;

// ==================== 常量 ====================

pub(crate) const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
pub(crate) const O_RDONLY: usize = 0;
pub(crate) const O_WRONLY: usize = 1;
pub(crate) const O_RDWR: usize = 2;
pub(crate) const O_CREAT: usize = 0x40;
pub(crate) const O_TRUNC: usize = 0x200;
pub(crate) const O_NONBLOCK: usize = 0x800;

pub(crate) const PROT_READ: usize = 1;
pub(crate) const PROT_WRITE: usize = 2;
pub(crate) const MAP_SHARED: usize = 1;

/// FBIOGET_VSCREENINFO 请求码（与内核 devfs 约定一致）。
pub(crate) const FBIOGET_VSCREENINFO: usize = 0x4600;
/// FBIO_PRESENT 请求码：把 canvas 脏区拷入扫描缓冲并翻页（应用驱动显示节奏）。
pub(crate) const FBIO_PRESENT: usize = 0x4603;

pub(crate) const FB_PATH: &str = "/dev/fb0";
pub(crate) const INPUT_PATH: &str = "/dev/input/event0";

/// linux input_event 事件类型 / 码。
pub(crate) const EV_KEY: u16 = 0x01;
pub(crate) const EV_REL: u16 = 0x02;
pub(crate) const EV_ABS: u16 = 0x03;
pub(crate) const ABS_X: u16 = 0x00;
pub(crate) const ABS_Y: u16 = 0x01;
pub(crate) const REL_X: u16 = 0x00;
pub(crate) const REL_Y: u16 = 0x01;
pub(crate) const REL_WHEEL: u16 = 0x08;
pub(crate) const BTN_TOUCH: u16 = 0x14a; // 330
pub(crate) const BTN_LEFT: u16 = 0x110; // 272
pub(crate) const KEY_ENTER: u16 = 28;
pub(crate) const KEY_BACKSPACE: u16 = 14;
pub(crate) const KEY_ESC: u16 = 1;
pub(crate) const KEY_LEFTSHIFT: u16 = 42;
pub(crate) const KEY_RIGHTSHIFT: u16 = 54;

// ==================== 显示与布局参数 ====================

pub(crate) const DISPLAY_W: usize = 540;
pub(crate) const DISPLAY_H: usize = 960;

pub(crate) const MARGIN: i32 = 16;
pub(crate) const CARD_W: i32 = DISPLAY_W as i32 - 2 * MARGIN;
pub(crate) const CARD_R: i32 = 16;
pub(crate) const ROW_H: i32 = 56;
pub(crate) const ICON_SZ: i32 = 24;
pub(crate) const ICON_R: i32 = 7;
pub(crate) const HEADER_H: i32 = 29;
pub(crate) const CARD_PAD: i32 = 12;
pub(crate) const BTN_H: i32 = 50;
pub(crate) const BTN_R: i32 = 12;

pub(crate) const TOP_TITLE_H: i32 = 44;
pub(crate) const TOP_BACK_X: i32 = 12;
pub(crate) const TOP_BACK_Y: i32 = 12;
pub(crate) const TOP_BACK_SZ: i32 = 36;

pub(crate) const SEARCH_X: i32 = 58;
pub(crate) const SEARCH_Y: i32 = 10;
pub(crate) const SEARCH_W: i32 = 470;
pub(crate) const SEARCH_H: i32 = 40;
pub(crate) const SEARCH_R: i32 = 20;

pub(crate) const LIST_TOP: i32 = 56;
pub(crate) const LIST_BOTTOM: i32 = sys_nav::NAV_Y;

// ==================== 颜色（0x00RRGGBB，ui.rs 复用；浅色主题） ====================

pub(crate) const C_SW_ON: u32 = 0x00_00_B9_6B;
pub(crate) const C_WHITE: u32 = 0x00_FF_FF_FF;
pub(crate) const C_BLACK: u32 = 0x00_00_00_00;
pub(crate) const C_PRIMARY: u32 = 0x00_00_7A_FF;
pub(crate) const C_DANGER: u32 = 0x00_FF_3B_30;
/// 次要/弱化文字（白底浅玻璃上可读的蓝灰）。
pub(crate) const C_SUB: u32 = 0x00_6B_72_80;

/// 页面基底背景（纯白；渐变由 ui.rs 在其上做白→极浅灰过渡）。
pub(crate) const T_PAGE_L: u32 = 0x00_FF_FF_FF;
/// 浅色主题固定：深色配置不再切换深色调色板（与浅色一致）。
pub(crate) const T_PAGE_D: u32 = 0x00_FF_FF_FF;
pub(crate) const T_CARD_L: u32 = 0x00_FF_FF_FF;
pub(crate) const T_CARD_D: u32 = 0x00_FF_FF_FF;
/// 主文字（白底/浅玻璃上）。
pub(crate) const T_TEXT_L: u32 = 0x00_1A_1C_22;
pub(crate) const T_TEXT_D: u32 = 0x00_1A_1C_22;
/// 分割线。
pub(crate) const T_DIV_L: u32 = 0x00_E5_E9_F0;
pub(crate) const T_DIV_D: u32 = 0x00_E5_E9_F0;
/// 输入框/字段填充（极浅灰）。
pub(crate) const T_FIELD_L: u32 = 0x00_F7_F8_FA;
pub(crate) const T_FIELD_D: u32 = 0x00_F7_F8_FA;
/// 开关/滑块未激活轨道（极浅灰）。
pub(crate) const T_SW_OFF_L: u32 = 0x00_E9_E9_EA;
pub(crate) const T_SW_OFF_D: u32 = 0x00_E9_E9_EA;

/// 浅色主题固定：ui.rs/pages2.rs 的玻璃与配色分支读取此值，始终走浅色
/// （白色磨砂玻璃 + 深色文字）；dark_mode 配置与开关逻辑保持不变。
pub(crate) fn dark_on() -> bool {
    false
}

// ==================== 全局状态 ====================

pub(crate) static mut FB_PTR: *mut u32 = core::ptr::null_mut();
/// 保留的 /dev/fb0 fd：FBIO_PRESENT 提交脏区需要（mmap 后页表已指向画布，fd 仅用于 ioctl）。
pub(crate) static mut FB_FD: isize = -1;
pub(crate) static mut FB_W: usize = DISPLAY_W;
pub(crate) static mut FB_H: usize = DISPLAY_H;
pub(crate) static mut FB_STRIDE_PX: usize = DISPLAY_W;
pub(crate) static mut BACK_BUF: [u32; DISPLAY_W * DISPLAY_H] = [0u32; DISPLAY_W * DISPLAY_H];
/// 页面转场快照：推入/弹出时保存的离场页整帧（2MB BSS，与 BACK_BUF 同级）。
pub(crate) static mut SNAP_BUF: [u32; DISPLAY_W * DISPLAY_H] = [0u32; DISPLAY_W * DISPLAY_H];

/// 已保存 Wi-Fi 网络清单（ssid）。
pub(crate) static mut SAVED_SSIDS: [[u8; 32]; 8] = [[0u8; 32]; 8];
pub(crate) static mut SAVED_COUNT: usize = 0;

/// /data/oobe/modules 中的应用清单（通知/权限页用）。
pub(crate) static mut APP_NAMES: [[u8; 24]; 24] = [[0u8; 24]; 24];
pub(crate) static mut APP_COUNT: usize = 0;

/// OTA 包清单。
pub(crate) static mut OTA_PKGS: [[u8; 32]; 4] = [[0u8; 32]; 4];
pub(crate) static mut OTA_COUNT: usize = 0;

/// 电池/电源状态缓存（syscall 410 打包 u64）。
pub(crate) static mut PM_STATUS: u64 = 0;

// ==================== 系统调用封装 ====================

#[inline(always)]
pub(crate) unsafe fn syscall1(n: usize, a1: usize) -> usize {
    let ret: usize;
    asm!("ecall", in("a7") n, inlateout("a0") a1 => ret, options(nostack));
    ret
}

#[inline(always)]
pub(crate) unsafe fn syscall2(n: usize, a1: usize, a2: usize) -> usize {
    let ret: usize;
    asm!("ecall", in("a7") n, inlateout("a0") a1 => ret, in("a1") a2, options(nostack));
    ret
}

#[inline(always)]
pub(crate) unsafe fn syscall3(n: usize, a1: usize, a2: usize, a3: usize) -> usize {
    let ret: usize;
    asm!(
        "ecall",
        in("a7") n,
        inlateout("a0") a1 => ret,
        in("a1") a2,
        in("a2") a3,
        options(nostack)
    );
    ret
}

#[inline(always)]
pub(crate) unsafe fn syscall6(n: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize, a6: usize) -> usize {
    let ret: usize;
    asm!(
        "ecall",
        in("a7") n,
        inlateout("a0") a1 => ret,
        in("a1") a2,
        in("a2") a3,
        in("a3") a4,
        in("a4") a5,
        in("a5") a6,
        options(nostack)
    );
    ret
}

// ==================== 基础 I/O ====================

pub(crate) unsafe fn write_fd(fd: usize, buf: &[u8]) -> usize {
    syscall3(SYS_WRITE, fd, buf.as_ptr() as usize, buf.len())
}

pub(crate) fn print(s: &str) {
    unsafe { write_fd(1, s.as_bytes()); }
}

pub(crate) fn print_dec(mut v: u32) {
    if v == 0 {
        print("0");
        return;
    }
    let mut buf = [0u8; 12];
    let mut i = buf.len();
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    unsafe { write_fd(1, &buf[i..]); }
}

pub(crate) fn sleep_ms(ms: usize) {
    let mut req = [0u8; 16];
    let sec = (ms / 1000) as u64;
    let nsec = ((ms % 1000) * 1_000_000) as u64;
    req[..8].copy_from_slice(&sec.to_le_bytes());
    req[8..].copy_from_slice(&nsec.to_le_bytes());
    unsafe { syscall2(SYS_NANOSLEEP, req.as_ptr() as usize, 0); }
}

pub(crate) unsafe fn exit(code: usize) -> ! {
    syscall1(SYS_EXIT, code);
    loop {}
}

/// 当前时间（微秒，自 epoch）。
pub(crate) fn now_us() -> u64 {
    let mut tv = [0u8; 16];
    unsafe {
        syscall2(SYS_GETTIMEOFDAY, tv.as_mut_ptr() as usize, 0);
        let sec = u64::from_le_bytes([
            tv[0], tv[1], tv[2], tv[3], tv[4], tv[5], tv[6], tv[7],
        ]);
        let usec = u64::from_le_bytes([
            tv[8], tv[9], tv[10], tv[11], tv[12], tv[13], tv[14], tv[15],
        ]);
        sec * 1_000_000 + usec
    }
}

// ==================== 文件操作与帧缓冲 ====================

pub(crate) static mut PATH_BUF: [u8; 128] = [0u8; 128];

pub(crate) unsafe fn open(path: &str, flags: usize) -> isize {
    let n = path.len().min(PATH_BUF.len() - 1);
    PATH_BUF[..n].copy_from_slice(&path.as_bytes()[..n]);
    PATH_BUF[n] = 0;
    syscall3(SYS_OPENAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, flags) as isize
}

pub(crate) unsafe fn read_file(path: &str, buf: &mut [u8]) -> isize {
    let fd = open(path, O_RDONLY);
    if fd < 0 {
        return -1;
    }
    let mut total = 0usize;
    while total < buf.len() {
        let n = syscall3(
            SYS_READ,
            fd as usize,
            buf.as_mut_ptr().add(total) as usize,
            buf.len() - total,
        ) as isize;
        if n <= 0 {
            break;
        }
        total += n as usize;
    }
    syscall1(SYS_CLOSE, fd as usize);
    total as isize
}

pub(crate) unsafe fn write_file(path: &str, data: &[u8]) -> bool {
    let fd = open(path, O_CREAT | O_TRUNC | O_WRONLY);
    if fd >= 0 {
        let n = write_fd(fd as usize, data);
        syscall1(SYS_CLOSE, fd as usize);
        n == data.len()
    } else {
        false
    }
}

/// 创建目录（mkdirat）。
pub(crate) unsafe fn mkdir_path(path: &str) -> bool {
    let n = path.len().min(PATH_BUF.len() - 1);
    PATH_BUF[..n].copy_from_slice(&path.as_bytes()[..n]);
    PATH_BUF[n] = 0;
    let r = syscall3(SYS_MKDIRAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, 0o755);
    // 0 = 成功；-17(EEXIST) 也可接受。
    (r as isize) == 0 || (r as isize) == -17
}

/// 列出目录文件名到 out（每项 \0 结尾），返回总长度。
pub(crate) unsafe fn list_dir(path: &str, out: &mut [u8]) -> usize {
    let fd = open(path, O_RDONLY);
    if fd < 0 {
        return 0;
    }
    let mut raw = [0u8; 2048];
    let mut nout = 0usize;
    loop {
        let n = syscall3(SYS_GETDENTS64, fd as usize, raw.as_mut_ptr() as usize, raw.len());
        if (n as isize) <= 0 {
            break;
        }
        let mut off = 0usize;
        while off + 19 <= n as usize {
            let reclen = u16::from_le_bytes([raw[off + 16], raw[off + 17]]) as usize;
            if reclen < 19 || off + reclen > n as usize {
                break;
            }
            let name = &raw[off + 19..off + reclen];
            let elen = name.iter().position(|&c| c == 0).unwrap_or(name.len());
            if elen > 0 {
                let skip = name[0] == b'.' && (elen == 1 || (elen == 2 && name[1] == b'.'));
                if !skip && nout + elen + 1 <= out.len() {
                    out[nout..nout + elen].copy_from_slice(&name[..elen]);
                    out[nout + elen] = 0;
                    nout += elen + 1;
                }
            }
            off += reclen;
        }
    }
    syscall1(SYS_CLOSE, fd as usize);
    nout
}

/// 打开 /dev/fb0、查询分辨率并 mmap。
pub(crate) fn fb_init() -> bool {
    unsafe {
        let fd = open(FB_PATH, O_RDWR);
        if fd < 0 {
            return false;
        }
        let mut info = [0u8; 24];
        let r = syscall3(SYS_IOCTL, fd as usize, FBIOGET_VSCREENINFO, info.as_mut_ptr() as usize);
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

/// 亮度/辅助功能 LUT：亮度 percent、反色、高对比度。
///
/// flip 整屏提交是每像素调用：`invert_on()`/`contrast_on()` 的配置查询与
/// 逐通道乘除在 518k 像素 × TCG 慢宿主下实测 ~4s（应用打开卡顿的最大单项）。
/// 三个参数都只有有限取值且变换逐通道独立，合成为一张 256 项通道 LUT，
/// 参数不变时直接复用 —— flip 主体降为查表 + 移位组合，输出逐位一致。
static mut FLIP_LUT: [u32; 256] = [0; 256];
static mut FLIP_LUT_KEY: (i32, bool, bool) = (-1, false, false);

fn flip_lut() -> (&'static [u32; 256], bool) {
    unsafe {
        let key = (
            cfg::brightness_level().max(1),
            cfg::invert_on(),
            cfg::contrast_on(),
        );
        if FLIP_LUT_KEY != key {
            let b = key.0 as u32;
            for v in 0..256u32 {
                let mut c = v * b / 100;
                if key.1 {
                    c = 255 - c;
                }
                if key.2 {
                    c = (((c as i32) - 128) * 3 / 2 + 128).clamp(0, 255) as u32;
                }
                FLIP_LUT[v as usize] = c;
            }
            FLIP_LUT_KEY = key;
        }
        (&FLIP_LUT, key.0 >= 100 && !key.1 && !key.2)
    }
}

/// 单像素经 LUT 变换（低频路径：非整屏对齐的逐行提交）。
fn lut_px(c: u32, lut: &[u32; 256]) -> u32 {    (lut[((c >> 16) & 0xFF) as usize] << 16)
        | (lut[((c >> 8) & 0xFF) as usize] << 8)
        | lut[(c & 0xFF) as usize]
}

fn brightness_level() -> i32 {
    cfg::brightness_level()
}

// ==================== 页面切换转场（鸿蒙页面转场规范：左右位移 + 缓动） ====================

/// 向黑色压暗 `a/255`。
fn shade(c: u32, a: u32) -> u32 {
    let r = ((c >> 16) & 0xFF) * a / 255;
    let g = ((c >> 8) & 0xFF) * a / 255;
    let b = (c & 0xFF) * a / 255;
    (r << 16) | (g << 8) | b
}

/// 把离场页整帧从 BACK_BUF 拷贝到 SNAP_BUF（推入/弹出前调用）。
pub(crate) fn snapshot_page() {
    unsafe {
        core::ptr::copy_nonoverlapping(
            BACK_BUF.as_ptr(),
            SNAP_BUF.as_mut_ptr(),
            DISPLAY_W * DISPLAY_H,
        );
    }
}

/// 页面转场合成（行级平移 + 快照补底 + 10px 接缝阴影）：
///   - push：新页（BACK_BUF）自右滑入，左移出屏幕的 `off` 列补旧页快照；
///   - pop：父页（BACK_BUF）左移入位，右侧 `off` 列被子页快照覆盖（随滑动右移出屏）。
/// `e` 为缓动后的进度 0..1，e >= 1 时无需合成。
pub(crate) fn compose_page_transition(e: f32, push: bool) {
    if e >= 1.0 {
        return;
    }
    unsafe {
        let w = DISPLAY_W;
        let off = ((1.0 - e) * w as f32) as usize;
        if off == 0 {
            return;
        }
        for y in 0..DISPLAY_H {
            let row = y * w;
            if push {
                for x in (off..w).rev() {
                    BACK_BUF[row + x] = BACK_BUF[row + x - off];
                }
                for x in 0..off {
                    BACK_BUF[row + x] = SNAP_BUF[row + x];
                }
                // 接缝阴影：落在旧页一侧，最强 90 沿 10px 线性衰减。
                for s in 0..10 {
                    if s < off {
                        let x = off - 1 - s;
                        BACK_BUF[row + x] = shade(BACK_BUF[row + x], (90 - 9 * s) as u32);
                    }
                }
            } else {
                for x in 0..w - off {
                    BACK_BUF[row + x] = BACK_BUF[row + x + off];
                }
                for x in (w - off)..w {
                    BACK_BUF[row + x] = SNAP_BUF[row + x - (w - off)];
                }
                // 接缝阴影：子页快照压在父页上的投影（父页一侧）。
                let c = w - off;
                for s in 0..10 {
                    if s < off && c > s {
                        let x = c - 1 - s;
                        BACK_BUF[row + x] = shade(BACK_BUF[row + x], (90 - 9 * s) as u32);
                    }
                }
            }
        }
    }
}

/// 转场期间的一帧：渲染新页 → 转场合成 → 导航栏/Toast → 翻转 → 光标 overlay。
pub(crate) fn full_redraw_transition(e: f32, push: bool) {
    unsafe { pages::render() };
    compose_page_transition(e, push);
    draw_nav_bar();
    draw_toast();
    flip();
    cursor_overlay::overlay_redraw(
        unsafe { FB_PTR },
        unsafe { FB_STRIDE_PX },
        unsafe { FB_W } as i32,
        unsafe { FB_H } as i32,
        unsafe { CUR_X },
        unsafe { CUR_Y },
    );
    // 转场帧 flip 的提交不含 overlay 光标像素：补光标小矩形呈现上屏。
    fb_present_rects(&[cursor_overlay::under_rect(
        unsafe { CUR_X },
        unsafe { CUR_Y },
    )]);
}

/// 把 back buffer 以 LUT 缩放后拷贝到显存（亮度/反色/高对比度即时生效）。
fn flip() {
    unsafe {
        if FB_PTR.is_null() {
            return;
        }
        // 亮度/反色/对比度合成通道 LUT（参数不变时复用）；identity 表示
        // 参数为默认直通（亮度 100、无反色/对比度），整屏走 memcpy 快路。
        let (lut, identity) = flip_lut();
        if FB_W == DISPLAY_W && FB_H == DISPLAY_H && FB_STRIDE_PX == DISPLAY_W {
            if identity {
                core::ptr::copy_nonoverlapping(BACK_BUF.as_ptr(), FB_PTR, DISPLAY_W * DISPLAY_H);
            } else {
                for i in 0..DISPLAY_W * DISPLAY_H {
                    *FB_PTR.add(i) = lut_px(BACK_BUF[i], lut);
                }
            }
        } else {
            let w = FB_W.min(DISPLAY_W);
            let h = FB_H.min(DISPLAY_H);
            if identity {
                for y in 0..h {
                    core::ptr::copy_nonoverlapping(
                        BACK_BUF.as_ptr().add(y * DISPLAY_W),
                        FB_PTR.add(y * FB_STRIDE_PX),
                        w,
                    );
                }
            } else {
                for y in 0..h {
                    for x in 0..w {
                        let v = BACK_BUF[y * DISPLAY_W + x];
                        *FB_PTR.add(y * FB_STRIDE_PX + x) = lut_px(v, lut);
                    }
                }
            }
        }
        fb_present();
    }
}

/// 提交整屏脏区并请求呈现（FBIO_PRESENT，单矩形 (0,0,FB_W,FB_H)）。
///
/// 内核在首次 FBIO_PRESENT 后进入应用驱动模式：不再由定时器自动翻页，
/// 显示节奏完全由本调用驱动（与 terminal 的 present_frame 同一最小实现）。
fn fb_present() {
    unsafe {
        if FB_FD < 0 || FB_PTR.is_null() {
            return;
        }
        let mut dbuf = [0u8; 4 + 16];
        // 缓冲布局：[count:u32][{x:i32, y:i32, w:u32, h:u32}]，全部小端。
        dbuf[0..4].copy_from_slice(&1u32.to_le_bytes());
        dbuf[4..8].copy_from_slice(&0i32.to_le_bytes());
        dbuf[8..12].copy_from_slice(&0i32.to_le_bytes());
        dbuf[12..16].copy_from_slice(&(FB_W as u32).to_le_bytes());
        dbuf[16..20].copy_from_slice(&(FB_H as u32).to_le_bytes());
        // 忽略返回值：内核在应用接管模式下始终呈现（无脏区时为 no-op）。
        syscall3(SYS_IOCTL, FB_FD as usize, FBIO_PRESENT, dbuf.as_ptr() as usize);
    }
}

/// 提交任意小脏矩形并请求呈现（FBIO_PRESENT，光标移动快速路径）。
///
/// 与整屏 [`fb_present`] 相比只提交 ~26x26 矩形：内核脏区拷贝与翻页都在
/// 微秒级完成，鼠标移动不再触发整屏 2MB 拷贝。
fn fb_present_rects(rects: &[(i32, i32, u32, u32)]) {
    unsafe {
        if FB_FD < 0 || FB_PTR.is_null() || rects.is_empty() {
            return;
        }
        let n = rects.len().min(8);
        let mut dbuf = [0u8; 4 + 8 * 16];
        dbuf[0..4].copy_from_slice(&(n as u32).to_le_bytes());
        for i in 0..n {
            let r = rects[i];
            let off = 4 + i * 16;
            dbuf[off..off + 4].copy_from_slice(&r.0.to_le_bytes());
            dbuf[off + 4..off + 8].copy_from_slice(&r.1.to_le_bytes());
            dbuf[off + 8..off + 12].copy_from_slice(&r.2.to_le_bytes());
            dbuf[off + 12..off + 16].copy_from_slice(&r.3.to_le_bytes());
        }
        syscall3(SYS_IOCTL, FB_FD as usize, FBIO_PRESENT, dbuf.as_ptr() as usize);
    }
}

// ==================== 按压反馈（iOS 按压高亮语义） ====================

/// 反馈半径（px）。
const PRESS_R: i32 = 26;

/// 按压点记录（-1 = 无活动反馈）。
static mut PRESS_FX: i32 = -1;
static mut PRESS_FY: i32 = -1;

/// 按下：触点画软边半透明压暗圆斑。直接写 canvas（不污染 BACK_BUF 权威
/// 底图）：拖动滚动时页面重绘自 BACK_BUF 自然覆盖反馈；抬起时从 BACK_BUF
/// 恢复该区域。此前设置页所有交互均无按压反馈（对比 OOBE 按钮反馈）。
pub(crate) unsafe fn press_feedback_down(x: i32, y: i32) {
    if FB_FD < 0 || FB_PTR.is_null() {
        return;
    }
    PRESS_FX = x;
    PRESS_FY = y;
    let r2 = PRESS_R * PRESS_R;
    for dy in -PRESS_R..=PRESS_R {
        for dx in -PRESS_R..=PRESS_R {
            let d2 = dx * dx + dy * dy;
            if d2 > r2 {
                continue;
            }
            let px = x + dx;
            let py = y + dy;
            if px < 0 || py < 0 || px >= DISPLAY_W as i32 || py >= DISPLAY_H as i32 {
                continue;
            }
            let idx = py as usize * DISPLAY_W + px as usize;
            let cur = *FB_PTR.add(idx);
            // 中心 ~35% 压暗，软边衰减
            let a = (90 * (r2 - d2) / r2) as u32;
            let r = (((cur >> 16) & 0xFF) as u32 * (255 - a)) / 255;
            let g = (((cur >> 8) & 0xFF) as u32 * (255 - a)) / 255;
            let b = ((cur & 0xFF) as u32 * (255 - a)) / 255;
            *FB_PTR.add(idx) = (r << 16) | (g << 8) | b;
        }
    }
    fb_present_rects(&[(x - PRESS_R, y - PRESS_R, (PRESS_R * 2) as u32, (PRESS_R * 2) as u32)]);
}

/// 抬起：从 BACK_BUF 恢复按压区域并呈现。
pub(crate) unsafe fn press_feedback_up() {
    let x = PRESS_FX;
    let y = PRESS_FY;
    PRESS_FX = -1;
    PRESS_FY = -1;
    if x < 0 || FB_FD < 0 || FB_PTR.is_null() {
        return;
    }
    let x0 = (x - PRESS_R).max(0);
    let y0 = (y - PRESS_R).max(0);
    let x1 = (x + PRESS_R).min(DISPLAY_W as i32 - 1);
    let y1 = (y + PRESS_R).min(DISPLAY_H as i32 - 1);
    if x1 < x0 || y1 < y0 {
        return;
    }
    // 恢复必须走与 flip 相同的 LUT（亮度/反色/对比度）—— 默认亮度 85 下
    // LUT 非恒等，裸拷 BACK_BUF 会在抬起处留下一块偏亮方块并踩掉光标
    // sprite，直到下一次整屏重绘。
    let (lut, _) = flip_lut();
    for py in y0..=y1 {
        for px in x0..=x1 {
            let idx = py as usize * DISPLAY_W + px as usize;
            *FB_PTR.add(idx) = lut_px(BACK_BUF[idx], lut);
        }
    }
    fb_present_rects(&[(x0, y0, (x1 - x0 + 1) as u32, (y1 - y0 + 1) as u32)]);
}

// ==================== 绘制原语 ====================

pub(crate) fn put_pixel(x: i32, y: i32, color: u32) {
    if x < 0 || y < 0 || x >= DISPLAY_W as i32 || y >= DISPLAY_H as i32 {
        return;
    }
    unsafe { BACK_BUF[y as usize * DISPLAY_W + x as usize] = color; }
}

pub(crate) fn fill_rect(x: i32, y: i32, w: i32, h: i32, color: u32) {
    if w <= 0 || h <= 0 {
        return;
    }
    // 先整体裁剪再逐行 slice::fill（宽通道 memset）：逐像素 put_pixel 的
    // 每像素 4 次边界检查降为每行一次。
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

/// BACK_BUF 抗锯齿绘制目标（settings 自有帧缓冲）。
struct SettingsSmooth;

impl sys_nav::glass::SmoothTarget for SettingsSmooth {
    fn put_aa(&mut self, x: i32, y: i32, color: u32, alpha: u32) {
        if x < 0 || y < 0 || x >= DISPLAY_W as i32 || y >= DISPLAY_H as i32 {
            return;
        }
        unsafe {
            let idx = y as usize * DISPLAY_W + x as usize;
            BACK_BUF[idx] = sys_nav::glass::blend(BACK_BUF[idx], color, alpha);
        }
    }
}

/// 填充圆角矩形（抗锯齿超椭圆连续曲率，ColorOS 15 观感）。
pub(crate) fn fill_round_rect(x: i32, y: i32, w: i32, h: i32, r: i32, color: u32) {
    sys_nav::glass::smooth_fill_rr(&mut SettingsSmooth, x, y, w, h, r, color);
}

/// 实心圆（抗锯齿）。
pub(crate) fn fill_circle(cx: i32, cy: i32, r: i32, color: u32) {
    sys_nav::glass::smooth_fill_circle(&mut SettingsSmooth, cx, cy, r, color);
}

pub(crate) fn draw_line(x0: i32, y0: i32, x1: i32, y1: i32, color: u32) {
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

pub(crate) fn draw_bits(x: i32, y: i32, bits: &[u8; 8], scale: i32, color: u32) {
    for row in 0..8 {
        let b = bits[row];
        for col in 0..8 {
            if b & (0x80 >> col) != 0 {
                for dy in 0..scale {
                    for dx in 0..scale {
                        put_pixel(x + col as i32 * scale + dx, y + row as i32 * scale + dy, color);
                    }
                }
            }
        }
    }
}

/// Phosphor-Bold 图标字形（粗线风格，笔画加粗 ~1px）按墨迹包围盒居中绘制到
/// (cx, cy)。灰度 alpha 与背景混合。图标字体未就绪返回 false，
/// 调用方退回 8x8 点阵兜底（draw_bits）。
pub(crate) fn draw_icon_glyph_centered(
    cx: i32,
    cy: i32,
    cp: u32,
    px: f32,
    color: u32,
) -> bool {
    if !ttf_text::font_render::icon_ready(false) {
        return false;
    }
    let Some(g) = ttf_text::font_render::rasterize_icon_thick(false, cp, px) else {
        return false;
    };
    if g.width == 0 || g.height == 0 {
        return false;
    }
    let x0 = cx - g.width as i32 / 2;
    let y0 = cy - g.height as i32 / 2;
    for row in 0..g.height {
        for col in 0..g.width {
            let a = g.data[row * g.width + col];
            if a == 0 {
                continue;
            }
            let gx = x0 + col as i32;
            let gy = y0 + row as i32;
            if gx < 0 || gy < 0 || gx >= DISPLAY_W as i32 || gy >= DISPLAY_H as i32 {
                continue;
            }
            unsafe {
                let bg = BACK_BUF[gy as usize * DISPLAY_W + gx as usize];
                put_pixel(gx, gy, ttf_text::font_render::blend(bg, color, a));
            }
        }
    }
    true
}

// ==================== TTF 平滑字体（cn.ttf，任意字号） ====================

pub(crate) fn ttf_glyph_px(x: i32, y: i32, cp: u32, px: f32, color: u32) -> i32 {
    if !ttf_text::font_render::font_ready() {
        return if cp >= 0x80 { 16 } else { 8 };
    }
    let ch = core::char::from_u32(cp).unwrap_or(' ');
    if let Some(g) = ttf_text::font_render::rasterize_char(ch, px) {
        let gx = x + g.xmin;
        for row in 0..g.height {
            for col in 0..g.width {
                let a = g.data[row * g.width + col];
                if a != 0 {
                    let px_ = gx + col as i32;
                    let py_ = y + row as i32;
                    if px_ >= 0 && py_ >= 0 && px_ < DISPLAY_W as i32 && py_ < DISPLAY_H as i32 {
                        let bg = unsafe { BACK_BUF[py_ as usize * DISPLAY_W + px_ as usize] };
                        put_pixel(px_, py_, ttf_text::font_render::blend(bg, color, a));
                    }
                }
            }
        }
        return g.advance;
    }
    0
}

pub(crate) fn draw_text_px(x: i32, y: i32, s: &[u8], px: f32, color: u32) {
    let mut cx = x;
    let mut i = 0;
    while i < s.len() {
        let (cp, n) = ttf_text::font_render::utf8_next(s, i);
        cx += ttf_glyph_px(cx, y, cp, px, color);
        i += n;
    }
}

pub(crate) fn text_width_px(s: &[u8], px: f32) -> i32 {
    ttf_text::font_render::text_width(s, px)
}

/// 以标题字体（bold.ttf）绘制单个码点。标题字体未就绪或缺少该字形时退回常规。
pub(crate) fn ttf_glyph_bold_px(x: i32, y: i32, cp: u32, px: f32, color: u32) -> i32 {
    if !ttf_text::font_render::has_glyph_bold(cp) {
        return ttf_glyph_px(x, y, cp, px, color);
    }
    let ch = core::char::from_u32(cp).unwrap_or(' ');
    if let Some(g) = ttf_text::font_render::rasterize_char_bold(ch, px) {
        let gx = x + g.xmin;
        for row in 0..g.height {
            for col in 0..g.width {
                let a = g.data[row * g.width + col];
                if a != 0 {
                    let px_ = gx + col as i32;
                    let py_ = y + row as i32;
                    if px_ >= 0 && py_ >= 0 && px_ < DISPLAY_W as i32 && py_ < DISPLAY_H as i32 {
                        let bg = unsafe { BACK_BUF[py_ as usize * DISPLAY_W + px_ as usize] };
                        put_pixel(px_, py_, ttf_text::font_render::blend(bg, color, a));
                    }
                }
            }
        }
        return g.advance;
    }
    0
}

/// 以标题字体（bold.ttf）按 UTF-8 绘制文本（标题栏等）。
pub(crate) fn draw_text_bold_px(x: i32, y: i32, s: &[u8], px: f32, color: u32) {
    let mut cx = x;
    let mut i = 0;
    while i < s.len() {
        let (cp, n) = ttf_text::font_render::utf8_next(s, i);
        cx += ttf_glyph_bold_px(cx, y, cp, px, color);
        i += n;
    }
}

/// 标题字体（bold.ttf）下的文本宽度。
pub(crate) fn text_width_px_bold(s: &[u8], px: f32) -> i32 {
    ttf_text::font_render::text_width_bold(s, px)
}

pub(crate) fn text_top(px: f32, cy: i32) -> i32 {
    if !ttf_text::font_render::font_ready() {
        return cy - (px as i32) / 2;
    }
    let (asc, desc) = ttf_text::font_render::line_metrics(px);
    cy - (asc - desc) / 2
}

// ==================== 工具函数 ====================

pub(crate) fn fmt_dec(buf: &mut [u8], mut v: u32) -> usize {
    if v == 0 {
        buf[0] = b'0';
        return 1;
    }
    let mut pos = buf.len();
    while v > 0 {
        pos -= 1;
        buf[pos] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    let n = buf.len() - pos;
    for i in 0..n {
        buf[i] = buf[pos + i];
    }
    n
}

pub(crate) fn starts_with(data: &[u8], prefix: &[u8]) -> bool {
    data.len() >= prefix.len() && data[..prefix.len()] == *prefix
}

pub(crate) fn find_sub(data: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || data.len() < needle.len() {
        return false;
    }
    for i in 0..=data.len() - needle.len() {
        if data[i..i + needle.len()] == *needle {
            return true;
        }
    }
    false
}

pub(crate) fn trim_space(data: &[u8]) -> &[u8] {
    let is_s = |c: u8| c == b' ' || c == b'\t' || c == b'\r' || c == b'\n';
    let mut s = 0;
    let mut e = data.len();
    while s < e && is_s(data[s]) {
        s += 1;
    }
    while e > s && is_s(data[e - 1]) {
        e -= 1;
    }
    &data[s..e]
}

pub(crate) fn parse_int(data: &[u8]) -> Option<i32> {
    let d = trim_space(data);
    if d.is_empty() {
        return None;
    }
    let mut v: i32 = 0;
    for &c in d {
        if c < b'0' || c > b'9' {
            return None;
        }
        v = v.saturating_mul(10).saturating_add((c - b'0') as i32);
    }
    Some(v)
}

pub(crate) fn is_sep(c: u8) -> bool {
    c == b' ' || c == b'\t' || c == b'\r' || c == b'\n'
}

pub(crate) fn trim_end(data: &[u8]) -> usize {
    let mut end = data.len();
    while end > 0 && (is_sep(data[end - 1]) || data[end - 1] == b',') {
        end -= 1;
    }
    end
}

pub(crate) fn count_tokens(data: &[u8]) -> u32 {
    let mut n = 0u32;
    let mut i = 0usize;
    while i < data.len() {
        while i < data.len() && (is_sep(data[i]) || data[i] == b',') {
            i += 1;
        }
        if i >= data.len() {
            break;
        }
        n += 1;
        while i < data.len() && data[i] != b',' {
            i += 1;
        }
    }
    n
}

// ==================== 输入 ====================

/// evdev 键码 → ASCII（QWERTY 物理布局，与 app_kit/oobe 同表）。
pub(crate) fn key_to_char(code: u16, shift: bool) -> Option<u8> {
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
        77 => b'@',
        _ => return None,
    };
    let c = if shift && c.is_ascii_lowercase() { c - 32 } else { c };
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
            b'.' => b'>',
            b',' => b'<',
            b'/' => b'?',
            _ => c,
        })
    } else {
        Some(c)
    }
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum KeyAction {
    None,
    Char(u8),
    Backspace,
    Enter,
    Esc,
}

pub(crate) struct PollRes {
    pub tap: Option<(i32, i32)>,
    pub nav: sys_nav::NavAction,
    pub moved: bool,
    pub redraw: bool,
    pub key: KeyAction,
}

static mut INPUT_FD: isize = -1;
pub(crate) static mut CUR_X: i32 = DISPLAY_W as i32 / 2;
pub(crate) static mut CUR_Y: i32 = DISPLAY_H as i32 / 2;
static mut TOUCH_DOWN: bool = false;
static mut TOUCH_PX: i32 = 0;
static mut TOUCH_PY: i32 = 0;
static mut TOUCH_US: u64 = 0;
/// 鼠标拖拽期间的 REL_Y 增量累计（drag_to 需要绝对 y，按下点 + 累计）。
static mut MOUSE_DRAG_ACC: i32 = 0;
static mut MOUSE_DOWN: bool = false;
static mut SHIFT: bool = false;

/// 轮询 /dev/input/event0，检测点击、拖拽滚动、滚轮、指针移动与物理键盘。
///
/// 返回：(点击坐标, 系统导航动作, 指针是否移动, 是否需要整屏重绘, 键盘动作)。
fn poll_input() -> PollRes {
    unsafe {
        if INPUT_FD < 0 {
            INPUT_FD = open(INPUT_PATH, O_RDONLY | O_NONBLOCK);
        }
        let mut res = PollRes {
            tap: None,
            nav: sys_nav::NavAction::None,
            moved: false,
            redraw: false,
            key: KeyAction::None,
        };
        if INPUT_FD < 0 {
            return res;
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
                        if MOUSE_DOWN {
                            // drag_to 期望「按下点为基准的绝对 y」：累计相对增量。
                            // 此前直接传单次 REL_Y 增量 → SCROLL 恒为
                            // SCROLL_AT_DOWN - (delta - PRESS_Y) ≈ 常量，按住
                            // 拖动立即钳到一端。仅变化时置 redraw（理由同 ABS_Y）。
                            MOUSE_DRAG_ACC = MOUSE_DRAG_ACC.saturating_add(value);
                            if unsafe { pages::drag_to(pages::press_y() + MOUSE_DRAG_ACC) } {
                                res.redraw = true;
                            }
                        }
                        CUR_Y = (CUR_Y + value).clamp(0, DISPLAY_H as i32 - 1);
                        res.moved = true;
                    } else if code == REL_WHEEL {
                        if unsafe { pages::wheel_by(value) } {
                            res.redraw = true;
                        }
                    }
                }
                EV_ABS => {
                    if code == ABS_X {
                        CUR_X = value.clamp(0, DISPLAY_W as i32 - 1);
                        res.moved = true;
                    } else if code == ABS_Y {
                        if TOUCH_DOWN {
                            // drag_to 期望绝对 y：此前传 TOUCH_PY - value，
                            // 展开后 SCROLL = SCROLL_AT_DOWN + value —— 滚动量
                            // 等于手指绝对坐标，按在屏幕下半区一碰就钳到底。
                            // 仅在实际滚动/滑条变化时置 redraw：无变化的重绘帧
                            // 会让主循环在导航检查前 continue，吞掉手势 nav。
                            if unsafe { pages::drag_to(value) } {
                                res.redraw = true;
                            }
                        }
                        CUR_Y = value.clamp(0, DISPLAY_H as i32 - 1);
                        res.moved = true;
                    }
                }
                EV_KEY => {
                    if code == BTN_TOUCH {
                        if value == 1 {
                            TOUCH_DOWN = true;
                            TOUCH_PX = CUR_X;
                            TOUCH_PY = CUR_Y;
                            TOUCH_US = now_us();
                            unsafe { pages::pointer_down(TOUCH_PX, TOUCH_PY); }
                            unsafe { press_feedback_down(TOUCH_PX, TOUCH_PY); }
                        } else if value == 0 && TOUCH_DOWN {
                            TOUCH_DOWN = false;
                            unsafe { press_feedback_up(); }
                            let sliding = unsafe { pages::pointer_up(TOUCH_PX, TOUCH_PY, CUR_X, CUR_Y) };
                            if !sliding {
                                let mode = sys_nav::nav_mode();
                                let a = sys_nav::nav_hit(CUR_X, CUR_Y, mode);
                                if a != sys_nav::NavAction::None {
                                    res.nav = a;
                                } else {
                                    let dur = now_us().saturating_sub(TOUCH_US) / 1000;
                                    let g = sys_nav::classify_swipe(
                                        TOUCH_PX, TOUCH_PY, CUR_X, CUR_Y, dur, mode,
                                    );
                                    if g != sys_nav::NavAction::None {
                                        res.nav = g;
                                    } else if (CUR_X - TOUCH_PX).abs() <= 12
                                        && (CUR_Y - TOUCH_PY).abs() <= 12
                                    {
                                        // 保留区内短拖（11~47px，不满足手势阈值）
                                        // 不得变成「抬起位置点击」—— 此前会误开
                                        // 列表项/开关。位移在 slop 内才视为点按。
                                        res.tap = Some((CUR_X, CUR_Y));
                                    }
                                }
                            }
                        }
                    } else if code == BTN_LEFT {
                        if value == 1 {
                            MOUSE_DOWN = true;
                            MOUSE_DRAG_ACC = 0;
                            // 与 BTN_TOUCH 共用同一套手势状态：QEMU GUI 的
                            // virtio-tablet 以 BTN_LEFT 上报，缺失这些字段
                            // 会让全面屏手势（底部上滑/边缘返回）在鼠标下失效。
                            TOUCH_PX = CUR_X;
                            TOUCH_PY = CUR_Y;
                            TOUCH_US = now_us();
                            unsafe { pages::pointer_down(CUR_X, CUR_Y); }
                            unsafe { press_feedback_down(CUR_X, CUR_Y); }
                        } else if value == 0 && MOUSE_DOWN {
                            MOUSE_DOWN = false;
                            unsafe { press_feedback_up(); }
                            let sliding = unsafe { pages::pointer_up(0, 0, CUR_X, CUR_Y) };
                            if !sliding {
                                let mode = sys_nav::nav_mode();
                                let a = sys_nav::nav_hit(CUR_X, CUR_Y, mode);
                                if a != sys_nav::NavAction::None {
                                    res.nav = a;
                                } else {
                                    let dur = now_us().saturating_sub(TOUCH_US) / 1000;
                                    let g = sys_nav::classify_swipe(
                                        TOUCH_PX, TOUCH_PY, CUR_X, CUR_Y, dur, mode,
                                    );
                                    if g != sys_nav::NavAction::None {
                                        res.nav = g;
                                    } else if (CUR_X - TOUCH_PX).abs() <= 12
                                        && (CUR_Y - TOUCH_PY).abs() <= 12
                                    {
                                        // 保留区内短拖（11~47px，不满足手势阈值）
                                        // 不得变成「抬起位置点击」—— 此前会误开
                                        // 列表项/开关。位移在 slop 内才视为点按。
                                        res.tap = Some((CUR_X, CUR_Y));
                                    }
                                }
                            }
                        }
                    } else if code == KEY_LEFTSHIFT || code == KEY_RIGHTSHIFT {
                        SHIFT = value == 1;
                    } else if code == KEY_BACKSPACE {
                        if value == 1 {
                            res.key = KeyAction::Backspace;
                        }
                    } else if code == KEY_ENTER {
                        if value == 1 {
                            res.key = KeyAction::Enter;
                        }
                    } else if code == KEY_ESC {
                        if value == 1 {
                            res.key = KeyAction::Esc;
                        }
                    } else if value == 1 {
                        if let Some(c) = key_to_char(code, SHIFT) {
                            res.key = KeyAction::Char(c);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// 轮询 /dev/tty（fd 0，标准输入）：检测 ESC（0x1b）按键。
fn poll_key() -> bool {
    // ★ 独立非阻塞 tty fd：fd 0 是阻塞语义（musl fgets），无数据时直读
    // 会把应用卡死在内核 yield 循环（tty_read 此前无视 O_NONBLOCK——
    // 内核已修，此处也换非阻塞 fd 双保险）。
    static mut TTY_POLL_FD: isize = -1;
    let fd = unsafe {
        if TTY_POLL_FD < 0 {
            let mut p = [0u8; 16];
            let s = b"/dev/tty\0";
            p[..s.len()].copy_from_slice(s);
            TTY_POLL_FD = syscall3(SYS_OPENAT, 0xFFFF_FFFF_FFFF_FF9Cusize, p.as_ptr() as usize, 0x800) as isize;
        }
        TTY_POLL_FD
    };
    if fd < 0 {
        return false;
    }
    let mut buf = [0u8; 8];
    let n = unsafe { syscall3(SYS_READ, fd as usize, buf.as_mut_ptr() as usize, buf.len()) };
    // 读失败 -1（usize 天文数字）按 isize 判负，否则越界 panic
    let n = n as isize;
    if n <= 0 {
        return false;
    }
    for i in 0..(n.min(buf.len() as isize) as usize) {
        if buf[i] == 0x1b {
            return true;
        }
    }
    false
}

// ==================== 渲染出口 ====================

fn full_redraw() {
    unsafe { pages::render(); }
    draw_nav_bar();
    draw_toast();
    flip();
    cursor_overlay::overlay_redraw(
        unsafe { FB_PTR },
        unsafe { FB_STRIDE_PX },
        unsafe { FB_W } as i32,
        unsafe { FB_H } as i32,
        unsafe { CUR_X },
        unsafe { CUR_Y },
    );
    // overlay 直接写画布，flip 的整屏提交不含光标像素；补一次光标小矩形
    // 呈现即可上屏（不再整屏重复提交）。
    fb_present_rects(&[cursor_overlay::under_rect(
        unsafe { CUR_X },
        unsafe { CUR_Y },
    )]);
}

/// 系统导航栏（三键 / 手势指示条，按当前模式绘制在屏幕底部，液态玻璃底）。
fn draw_nav_bar() {
    struct SettingsCanvas;
    impl sys_nav::NavCanvas for SettingsCanvas {
        fn put(&mut self, x: i32, y: i32, color: u32) {
            put_pixel(x, y, color);
        }
        fn backdrop(&self, x: i32, y: i32) -> u32 {
            if x >= 0 && y >= 0 && x < DISPLAY_W as i32 && y < DISPLAY_H as i32 {
                unsafe { BACK_BUF[y as usize * DISPLAY_W + x as usize] }
            } else {
                0
            }
        }
    }
    let mut cv = SettingsCanvas;
    sys_nav::draw_nav_bar(&mut cv, sys_nav::nav_mode());
}

/// 居中圆角 toast（液态玻璃药丸；错误提示与操作反馈统一绘制）。
fn draw_toast() {
    if !err_ui::err_active(now_us()) {
        return;
    }
    let msg = err_ui::err_msg();
    if msg.is_empty() {
        return;
    }
    let w = (text_width_px(msg, 14.0) + 56).clamp(80, 460);
    let x = (DISPLAY_W as i32 - w) / 2;
    let y = 96;
    ui::draw_glass(x, y, w, 40, 20);
    // 浅色玻璃药丸上用深色文字（白字在浅玻璃上不可读）。
    draw_text_px(x + 28, text_top(14.0, y + 20), msg, 14.0, c_text());
}

/// 设置一条用户可见的提示（自动消失）。
fn show_error(msg: &[u8]) {
    err_ui::err_set(msg, now_us());
}

pub(crate) fn toast(msg: &[u8]) {
    show_error(msg);
}

// ==================== 主流程 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[settings] 启动中
");

    if !fb_init() {
        print("[settings] /dev/fb0 内存映射失败，无帧缓冲
");
        unsafe { exit(1); }
    }
    print("[settings] 帧缓冲 ");
    print_dec(unsafe { FB_W } as u32);
    print("x");
    print_dec(unsafe { FB_H } as u32);
    print(" 已映射\n");

    // 只做一次首帧重绘：字体加载完成后再绘制。此前的「先画一帧再加载字体、
    // 加载完再画一帧」会让启动期整屏提交翻倍（整屏 flip 是应用启动最大的
    // 单项开销）；字体加载期间屏幕由 launcher 的启动占位页覆盖，无黑窗。
    unsafe { cfg::settings_init(); }

    ttf_text::font_init();
    // 标题字体：font_init 已改为按需加载（各应用启动少解析一份 ~220KB 子集），
    // 设置页标题/对话框标题走粗体，这里显式加载。
    ttf_text::font_init_bold();
    full_redraw();
    print("[settings] 就绪
");
    // 首屏主列表入场错峰（P0-6）：仅在启动时播放一次，滚动/翻页不重播。
    unsafe { pages::play_root_enter(10); }

    loop {
        let res = poll_input();
        // 系统导航动作最优先：必须先于 redraw 的 continue 分支 —— 否则同一
        // 批事件里只要有一个移动帧置了 redraw，上滑回桌面/边缘返回的 nav
        // 就会被整屏重绘分支直接丢弃（全面屏手势失效的根因）。
        if res.nav != sys_nav::NavAction::None {
            let code = sys_nav::nav_exit_code(res.nav);
            print("[settings] 导航操作 → 退出
");
            unsafe { exit(code); }
        }
        // 键盘动作。
        if res.key != KeyAction::None {
            match unsafe { pages::handle_key(res.key) } {
                KeyEffect::Handled => {
                    full_redraw();
                    continue;
                }
                KeyEffect::Exit => {
                    print("[settings] ESC → 退出
");
                    break;
                }
                KeyEffect::Ignore => {}
            }
        }
        if let Some((x, y)) = res.tap {
            print("[settings] 点击（");
            print_dec(x as u32);
            print(",");
            print_dec(y as u32);
            print("）
");
            let effect = unsafe { pages::handle_tap(x, y) };
            match effect {
                TapEffect::Redraw => {
                    full_redraw();
                    continue;
                }
                TapEffect::Exit => {
                    print("[settings] 页面退出
");
                    break;
                }
                TapEffect::None => {}
            }
        }
        if res.redraw {
            full_redraw();
            continue;
        }
        if res.moved {
            // 指针移动：overlay 快速路径只更新光标小区域，并把新旧位置区
            // 以小脏矩形提交呈现（不再整屏提交 —— 整屏拷贝是移动卡顿根源）。
            let rects = cursor_overlay::overlay_move_rects(
                unsafe { FB_PTR },
                unsafe { FB_STRIDE_PX },
                unsafe { FB_W } as i32,
                unsafe { FB_H } as i32,
                unsafe { CUR_X },
                unsafe { CUR_Y },
            );
            fb_present_rects(&rects);
        }
        if poll_key() {
            print("[settings] ESC → 退出
");
            break;
        }
        sleep_ms(16);
    }

    print("[settings] 正在退出
");
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
        write_fd(2, b"PANIC: settings\n");
        write_fd(2, &w.buf[..w.len]);
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}