//! 用户态 TTF 平滑字体基础设施（自包含：svc 系统调用 + brk 堆 + kunxfont）。
//!
//! 各 EL0 应用以 `#[path = "../../../shared/ttf_text.rs"] mod ttf_text;` 引入：
//! - [`alloc`] 可用（本模块注册 kunxrt 可回收 [`#[global_allocator]`]）；
//! - [`font_init`] 从 `/fonts/cn.ttf`（rootfs 中的 cn.ttf 子集）加载常规字体，
//!   并从 `/fonts/bold.ttf` 加载标题字体；
//! - [`font_render`] 提供栅格化/度量/混合原语，应用自行把灰度混合到帧缓冲。

extern crate alloc;

kunxrt::allocator!();

#[path = "font_render.rs"]
pub mod font_render;

// ==================== 系统调用（AArch64 asm-generic） ====================

const SYS_READ: usize = 63;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_WRITE: usize = 64;
const SYS_LSEEK: usize = 62;
const SYS_NANOSLEEP: usize = 101;
const SYS_GETTIMEOFDAY: usize = 169;
const SYS_CLOCK_GETTIME: usize = 113;
const CLOCK_MONOTONIC: i32 = 1;
const SYS_EXIT: usize = 93;

const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;

use kunxrt::syscall::syscall1;

use kunxrt::syscall::syscall2;

use kunxrt::syscall::syscall3;

// ==================== 启动门控 ====================
//
// launcher 在启动动画开始时 fork+execve 应用（应用加载与动画并行），动画
// 结束（白闪覆盖屏幕）前应用需等待，避免"动画播完才加载应用"导致的卡顿。
// 门控文件 /data/oobe/.launch_gate：launcher 动画期间写入 "0"（等待），
// 动画完成写入 "1"（放行）。应用侧在 font_init 后等待该文件变为 "1" 再
// 绘制首帧；文件不存在（如直接运行）则立即放行。

const GATE_PATH: &[u8] = b"/data/oobe/.launch_gate\0";

fn sleep_ms(ms: usize) {
    let mut req = [0u8; 16];
    let sec = (ms / 1000) as u64;
    let nsec = ((ms % 1000) * 1_000_000) as u64;
    req[..8].copy_from_slice(&sec.to_le_bytes());
    req[8..].copy_from_slice(&nsec.to_le_bytes());
    unsafe {
        syscall2(SYS_NANOSLEEP, req.as_ptr() as usize, 0);
    }
}

fn now_us() -> u64 {
    // 单调钟：RTC 就绪前后墙钟会跳变，时间差判定（动画/超时）必须用
    // CLOCK_MONOTONIC（黑屏事故同族防御）。
    let mut tv = [0u8; 16];
    unsafe {
        syscall3(SYS_CLOCK_GETTIME, CLOCK_MONOTONIC as usize, tv.as_mut_ptr() as usize, 0);
    }
    let sec = u64::from_le_bytes([tv[0], tv[1], tv[2], tv[3], tv[4], tv[5], tv[6], tv[7]]);
    let nsec = u64::from_le_bytes([tv[8], tv[9], tv[10], tv[11], tv[12], tv[13], tv[14], tv[15]]);
    sec * 1_000_000 + nsec / 1_000
}
/// 等待 launcher 启动动画完成（门控放行）。文件不存在或内容为 "1" 立即返回；
/// 内容为 "2" 表示 launcher 取消了本次启动（上划打断），应用应直接退出。
pub fn wait_launch_gate() {
    // 最多等待 3 秒，避免异常情况下应用永久挂起。
    let deadline = now_us() + 3_000_000;
    loop {
        let fd = unsafe { syscall3(SYS_OPENAT, AT_FDCWD, GATE_PATH.as_ptr() as usize, 0) };
        if (fd as isize) < 0 {
            return; // 文件不存在：非 launcher 启动，直接放行
        }
        let mut buf = [0u8; 4];
        let n = unsafe { syscall3(SYS_READ, fd as usize, buf.as_mut_ptr() as usize, 4) };
        unsafe { syscall1(SYS_CLOSE, fd as usize) };
        if n == 1 && buf[0] == b'1' {
            return; // launcher 已放行
        }
        if n == 1 && buf[0] == b'2' {
            // launcher 取消启动（图标飞出）：应用在绘制前退出。
            unsafe { syscall1(SYS_EXIT, 1) };
        }
        if now_us() >= deadline {
            return; // 超时保护
        }
        sleep_ms(5);
    }
}

// ==================== 字体加载 ====================

const FONT_PATH: &[u8] = b"/fonts/cn.ttf\0";
/// 标题字体路径（bold.ttf 子集，由 fonts/bold.ttf 经 make_fonts.py 生成）。
const BOLD_FONT_PATH: &[u8] = b"/fonts/bold.ttf\0";
/// OOBE 向导专属超小子集（仅向导文案，~380KB/867 字形，由 uiputong/bold 经
/// make_fonts.py 生成）：体积只有全量子集的 1/4，fontdue 解析在 TCG 慢宿主下
/// 也只需秒级，让向导首帧就直接平滑渲染，不再出现像素兜底。
const OOBE_CN_PATH: &[u8] = b"/fonts/oobe-cn.ttf\0";
const OOBE_BOLD_PATH: &[u8] = b"/fonts/oobe-bold.ttf\0";
/// 桌面启动器专属子集（launcher-cn.ttf 正文／launcher-bold.ttf 标题，由
/// make_fonts.py 生成，仅桌面文案 ~947 字形）。体积只有全量子集的 1/4——
/// 桌面首帧即平滑，「正在进入系统」不再在 TCG 慢宿主上卡住等字体。
const LAUNCHER_CN_PATH: &[u8] = b"/fonts/launcher-cn.ttf\0";
const LAUNCHER_BOLD_PATH: &[u8] = b"/fonts/launcher-bold.ttf\0";
/// Phosphor 图标字体（码点表见 shared/icons.rs）：icon-fill.ttf 为
/// Phosphor-Fill 实心（独立图标绘制），icon-line.ttf 为 Phosphor-Bold 粗线
/// （行内 emoji）。make_fonts.py 子集（仅 icons.rs 出现的码点）。
const ICON_FILL_PATH: &[u8] = b"/fonts/icon-fill.ttf\0";
const ICON_LINE_PATH: &[u8] = b"/fonts/icon-line.ttf\0";

/// 读取整个文件到 Vec。失败返回空 Vec。
fn read_file(path: &[u8]) -> alloc::vec::Vec<u8> {
    let fd = unsafe { syscall3(SYS_OPENAT, AT_FDCWD, path.as_ptr() as usize, 0) };
    if (fd as isize) < 0 {
        return alloc::vec::Vec::new();
    }
    let size = unsafe { syscall3(SYS_LSEEK, fd as usize, 0, 2) } as isize; // SEEK_END
    unsafe { syscall3(SYS_LSEEK, fd as usize, 0, 0) }; // SEEK_SET
    // ★ lseek 失败（-errno）按 isize 判负并钳制：usize::MAX 直接
    // with_capacity(usize::MAX) → capacity overflow abort（字体读取失败
    // 从优雅降级变成进程死亡）。
    let size = if size <= 0 { 0usize } else { (size as usize).min(64 * 1024 * 1024) };
    let mut data: alloc::vec::Vec<u8> = alloc::vec::Vec::with_capacity(size);
    // 32KB 分块：内核 SFS 的 fs_read 单次调用内部会跨块循环填充整个用户
    // 缓冲，块越大 syscall 越少（1.6MB 字体从 400 次降至 ~50 次）。
    let mut buf = [0u8; 32 * 1024];
    loop {
        let n = unsafe { syscall3(SYS_READ, fd as usize, buf.as_mut_ptr() as usize, buf.len()) };
        if n == 0 || (n as isize) < 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
    }
    unsafe {
        syscall1(SYS_CLOSE, fd as usize);
    }
    data
}

/// 从 rootfs 的 `/fonts/bold.ttf` 加载标题字体（幂等）。成功返回 true。
pub fn font_init_bold() -> bool {
    if font_render::font_bold_ready() {
        return true;
    }
    let data = read_file(BOLD_FONT_PATH);
    if data.is_empty() {
        return false;
    }
    match font_render::font_from_bytes(&data) {
        Some(f) => {
            font_render::font_set_bold(f);
            true
        }
        None => false,
    }
}

/// OOBE 启动路径专用：只加载向导超小子集（oobe-cn.ttf 正文／oobe-bold.ttf
/// 标题，同源于 uiputong/bold 更纱黑体子集）。体积小 → QEMU TCG 慢宿主下
/// 首帧即平滑，无需像素兜底。桌面与其他应用仍用全量子集（font_init）。
/// Phosphor 图标字体一并加载（向导步骤图标矢量绘制，替换像素几何图标）。
pub fn font_init_oobe() -> bool {
    let ready = font_render::font_ready();
    if !ready {
        let data = read_file(OOBE_CN_PATH);
        if !data.is_empty() {
            if let Some(f) = font_render::font_from_bytes(&data) {
                font_render::font_set(f);
            }
        }
    }
    if !font_render::font_bold_ready() {
        let data = read_file(OOBE_BOLD_PATH);
        if !data.is_empty() {
            if let Some(f) = font_render::font_from_bytes(&data) {
                font_render::font_set_bold(f);
            }
        }
    }
    font_init_icons();
    font_render::lazy_full_enable();
    font_render::font_ready()
}

/// 桌面启动器专用：加载超大子集（launcher-cn.ttf 正文，仅桌面文案 ~947 字形）。
/// 桌面首帧即平滑，慢 TCG 宿主不再卡在「正在进入系统」。
/// Phosphor 图标字体一并加载（行内 emoji / 天气小组件图标）。
/// 桌面未使用粗体字形，标题字体不再随启动加载（见 [`font_init`] 懒加载说明）。
pub fn font_init_launcher() -> bool {
    let ready = font_render::font_ready();
    if !ready {
        let data = read_file(LAUNCHER_CN_PATH);
        if !data.is_empty() {
            if let Some(f) = font_render::font_from_bytes(&data) {
                font_render::font_set(f);
            }
        }
    }
    font_init_icons();
    font_render::lazy_full_enable();
    font_render::font_ready()
}

/// 从 rootfs 加载 Phosphor 图标字体（icon-fill/icon-line，幂等）。
/// 失败不影响文字渲染：行内 emoji 退回常规字体路径，独立图标由调用方兜底。
pub fn font_init_icons() {
    if !font_render::icon_ready(true) {
        let data = read_file(ICON_FILL_PATH);
        if !data.is_empty() {
            if let Some(f) = font_render::font_from_bytes(&data) {
                font_render::font_set_icon(true, f);
            }
        }
    }
    if !font_render::icon_ready(false) {
        let data = read_file(ICON_LINE_PATH);
        if !data.is_empty() {
            if let Some(f) = font_render::font_from_bytes(&data) {
                font_render::font_set_icon(false, f);
            }
        }
    }
}

/// 打印 `[perf] font_init <ms>ms`（应用启动字体阶段诊断行，每次打开一行）。
fn perf_font_ms(total_ms: u64) {
    let mut buf = [0u8; 40];
    let mut n = 0;
    for &c in b"[perf] font_init " {
        buf[n] = c;
        n += 1;
    }
    let mut v = total_ms;
    let mut digits = [0u8; 12];
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
    for &c in b"ms\n" {
        buf[n] = c;
        n += 1;
    }
    unsafe {
        syscall3(SYS_WRITE, 1, buf.as_ptr() as usize, n);
    }
}

/// 从 rootfs 的 `/fonts/cn.ttf` 加载常规字体（幂等；常规字体加载完成后等待
/// launcher 启动动画的门控，与动画并行启动时）。
///
/// 标题字体（bold.ttf，~220KB）改为**按需加载**：应用启动的最大耗时项是
/// fontdue 解析，而 21 个应用中仅 settings/store 使用粗体 —— 全量应用
/// 逐一解析一份用不到的子集是纯浪费。需要粗体的应用在启动时显式调用
/// [`font_init_bold`]；未加载时粗体绘制自动回退常规字形（font_render）。
pub fn font_init() -> bool {
    let t0 = now_us();
    let ready = font_render::font_ready();
    if !ready {
        let data = read_file(FONT_PATH);
        if !data.is_empty() {
            if let Some(f) = font_render::font_from_bytes(&data) {
                font_render::font_set(f);
            }
        }
        drop(data);
    }
    // Phosphor 图标字体（emoji/图标）；失败同样不影响文字渲染。
    font_init_icons();
    // 子集字体按「源码字符串字面量」裁剪（构建期），运行期拼出的字符
    // （用户数据等）可能缺字形：开启一次性全量字体兜底（font_render）。
    font_render::lazy_full_enable();
    if !font_render::font_ready() {
        return false;
    }
    let ms = (now_us().saturating_sub(t0)) / 1000;
    if ms > 0 {
        // 仅在实际发生字体加载时打印（clock 等应用每帧做 font_init 热重载
        // 检查，幂等 0ms 路径打印会刷屏串口日志）。
        perf_font_ms(ms);
    }
    // 字体加载完成（耗时部分与启动动画并行），等 launcher 放行后再绘制首帧。
    wait_launch_gate();
    true
}
