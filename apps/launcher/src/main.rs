// KunxOS launcher：桌面启动器（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由内核 init 经 spawn_service("/bin/launcher") 拉起。职责：
//   1. 打开 /dev/fb0（openat）并 mmap 到用户空间，直接写入 RGBA8888 像素
//   2. 桌面：4x6 网格（可切换 5x6）+ 固定 Dock（电话/短信/浏览器/相机/设置）
//      + 小组件（时钟/天气/日历/相册/电池）+ 系统工具文件夹（3x3、可重命名）
//   3. 轮询 /dev/input/event0（触摸事件）；设备未就绪时回退为固定坐标模拟点击
//   4. 交互：点击启动应用；长按 500ms 进入编辑模式（图标 2° 抖动、拖动换位、
//      点击移除、网格切换）；文件夹 350ms 弹簧缩放打开
//
// 说明：内核已实现用户态 fork（clone(SIGCHLD)）/ execve / wait4，因此点击
// 图标会真正拉起 /bin/calculator、/bin/settings 等独立进程；应用自己 open/mmap
// /dev/fb0 绘制全屏窗口，退出后由 launcher 重绘桌面。framebuffer 由内核在启动时
// 初始化（gpu_simple::fb_init），launcher 与各应用直接 open/mmap /dev/fb0，
// 无需 displayd 合成器与 FLUSH 命令。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;
#[path = "../../../shared/ttf_text.rs"]
mod ttf_text;

// 共享系统导航（全面屏手势 / 三键虚拟按键）。
#[path = "../../../shared/nav.rs"]
mod sys_nav;

// 共享运动库（贝塞尔缓动 + 弹簧，docs/animations.md 应用侧实现）。
#[path = "../../../shared/motion.rs"]
mod motion;

use sys_nav::*;

// 控制中心（右上角下拉；含与通知中心的手势区分）。
mod control_center;

// 共享 CJK 16x16 点阵字库（GB2312 一级，由 tools/gen_cjk_font.py 生成）。
#[path = "../../../shared/cjk_font_launcher.rs"]
mod cjk_font;

// 系统鼠标指针贴图（tubiao/shubiao.png 生成，24x24 + 热点）。
#[path = "../../../shared/cursor_sprite.rs"]
mod cursor_sprite;

// 桌面应用图标贴图（tools/make_icons.py 生成，56x56 × 21，与 APPS 表同序；
// 随 rootfs 以 /share/app_icons.rgba 提供，启动时读入——内嵌会顶爆内核
// execve 缓冲上限 EXEC_BUF_CAP）。
#[path = "../../../shared/app_icons_sprite.rs"]
mod app_icons;

// 烘焙贴图边长必须等于图标瓦片边长（调整 ICON_SIZE 后需重跑 make_icons.py）。
const _: () = assert!(ICON_SIZE as usize == app_icons::ICON_PX);

// 应用启动动画与并行加载（点击反馈 / 启动动画 / 占位页 / 返回动画）。
mod app_launch;
use app_launch::{
    app_is_flying, cancel_press, launch_blit, motion_advance, poll_cancel_reap, start_fly_away,
    start_launch, start_press, zoom_blit, Phase, Spring, MOTION,
};

// 生命周期：launcher × lifd 集成（非阻塞应用管理：后台化/恢复/回收）。
mod app_life;
use app_life::{poll_background_events, poll_foreground};

// ==================== 系统调用号（AArch64 asm-generic） ====================

const SYS_READ: usize = 63;
const SYS_WRITE: usize = 64;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_IOCTL: usize = 29;
const SYS_EXIT: usize = 93;
const SYS_NANOSLEEP: usize = 101;
const SYS_CLONE: usize = 220;
const SYS_EXECVE: usize = 221;
const SYS_GETTIMEOFDAY: usize = 169;
const SYS_MMAP: usize = 222;
const SYS_WAIT4: usize = 260;
const SYS_SETUID: usize = 146;
/// per-app UID 基址（Android 惯例：应用 UID 从 10000 起）。launcher 以 root
/// 运行，fork 出的应用子进程在 execve 前 setuid 到该值——应用进程从此
/// 受内核 kill 权限检查约束（非 root 不能向 launcher/init/系统服务或其它
/// 应用发信号），构成最小沙箱。
const APP_UID: usize = 10000;

// ==================== 常量 ====================

const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C; // -100
const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const O_RDWR: usize = 2;
const O_CREAT: usize = 0x40;
const O_TRUNC: usize = 0x200;
const O_NONBLOCK: usize = 0x800;

/// mmap 保护位与标志。
const PROT_READ: usize = 1;
const PROT_WRITE: usize = 2;
const MAP_SHARED: usize = 1;

/// FBIOGET_VSCREENINFO 请求码（与内核 devfs 约定一致）。
const FBIOGET_VSCREENINFO: usize = 0x4600;

/// FBIO_FADE_SPLASH 请求码：触发内核侧启动画面线性淡出（首帧渲染完成后调用）。
const FBIO_FADE_SPLASH: usize = 0x4601;

/// FBIO_PRESENT：提交变更矩形并请求页翻转（应用直绘 → 内核脏区域合成器）。
const FBIO_PRESENT: usize = 0x4603;

/// 单帧脏矩形上限（与内核 MAX_DIRTY 一致；超限退化为整屏）。
const MAX_DIRTY: usize = 32;

/// clone 标志：SIGCHLD（标准 fork 语义）。
const SIGCHLD: usize = 17;

/// 帧缓冲设备与输入设备路径。
const FB_PATH: &str = "/dev/fb0";
const INPUT_PATH: &str = "/dev/input/event0";

/// 路径缓冲上限。
const PATH_CAP: usize = 96;

// ==================== 显示参数 ====================

/// 逻辑分辨率（与 displayd / ramfb 默认一致）。
const DISPLAY_W: usize = 540;
const DISPLAY_H: usize = 960;

/// 状态栏高度（顶部）。
const STATUS_H: i32 = 24;

/// 网格系统：默认 4 列 x 6 行，可切换 5 列 x 6 行。
const GRID_ROWS: i32 = 6;
const GRID_COLS_DEFAULT: i32 = 4;
/// 图标 56x56，圆角 14px（ColorOS 风格圆角矩形）。
const ICON_SIZE: i32 = 56;
/// 图标水平间距 24px / 垂直间距 20px。
const ICON_H_GAP: i32 = 24;
const ICON_V_GAP: i32 = 20;
const LABEL_H: i32 = 18;
const CELL_W: i32 = ICON_SIZE + ICON_H_GAP; // 80
const CELL_H: i32 = ICON_SIZE + LABEL_H + ICON_V_GAP; // 94

/// 底部固定 Dock（导航栏之上）。
const DOCK_H: i32 = 64;
const DOCK_Y: i32 = NAV_Y - DOCK_H;
const DOCK_ICON: i32 = 52;
const DOCK_GAP: i32 = 22;

/// 网格顶边：状态栏与 Dock 之间垂直居中。
const GRID_Y: i32 = STATUS_H + (DOCK_Y - STATUS_H - GRID_ROWS * CELL_H) / 2;

fn grid_x(cols: i32) -> i32 {
    (DISPLAY_W as i32 - (cols * CELL_W - ICON_H_GAP)) / 2
}

/// 模拟点击延迟（微秒）：输入设备未就绪时的固定坐标模拟点击。
const SIM_CLICK_US: u64 = 3_000_000;

// ==================== 颜色（0x00RRGGBB，浅色主题：白色背景打底） ====================

/// 页面基底背景（纯白）。
const COLOR_BG: u32 = 0x00_FF_FF_FF;
/// 状态栏/面板浅灰底。
const COLOR_STATUS_BG: u32 = 0x00_F2_F4_F8;
/// 彩色表面（强调绿/危险红玻璃、深色压暗层）上的文字保持白色。
const COLOR_TEXT: u32 = 0x00_FF_FF_FF;
/// 主文字：白底/浅玻璃上的深色文字。
const COLOR_TEXT_MAIN: u32 = 0x00_1A_1C_22;
/// 次要/弱化文字。
const COLOR_TEXT_DIM: u32 = 0x00_6B_72_80;
/// 卡片/按钮描边。
const COLOR_BORDER: u32 = 0x00_D9_DE_E6;
/// 分割线。
const COLOR_DIVIDER: u32 = 0x00_E5_E9_F0;
const COLOR_ICON_BORDER: u32 = 0x00_FF_FF_FF;
const COLOR_PANEL: u32 = 0x00_FF_FF_FF;
const COLOR_PANEL_ROW: u32 = 0x00_F7_F8_FA;
const COLOR_ACCENT: u32 = 0x00_00_B9_6B;
const COLOR_WARN: u32 = 0x00_E7_4C_3C;
const COLOR_DIM: u32 = 0x00_6B_72_80;
const COLOR_FOLDER: u32 = 0x00_E8_A0_2D;

/// 浅色磨砂玻璃（launcher 本地自定义样式）：白色着色 + 浅灰描边，
/// 替代自绘深色玻璃（如"通话中"横幅）。
const GLASS_FROST: GlassStyle = GlassStyle {
    tint: 0x00_FF_FF_FF,
    alpha: 225,
    spec: 60,
    sheen: 18,
    border: 0x00_D9_DE_E6,
    border_a: 220,
};

const THEME_PATH: &str = "/data/oobe/theme";
const LANG_PATH: &str = "/data/oobe/lang";
const WIFI_PATH: &str = "/data/oobe/wifi_enabled";
const BRIGHTNESS_PATH: &str = "/data/oobe/brightness";

// ==================== 应用表 ====================

struct App {
    key: &'static str,
    name: &'static str,
    name_zh: &'static str,
    path: &'static str,
    color: u32,
    default_on: bool,
}

impl App {
    /// 按当前语言返回显示名（默认中文，`/data/oobe/lang` 含 "zh" 或为空时用中文）。
    fn display_name(&self) -> &'static str {
        if unsafe { LANG_ZH } {
            self.name_zh
        } else {
            self.name
        }
    }
}

static APPS: [App; 21] = [
    App {
        key: "phone",
        name: "Phone",
        name_zh: "电话",
        path: "/bin/phone",
        color: 0x00_2E_86_C1,
        default_on: true,
    },
    App {
        key: "sms",
        name: "SMS",
        name_zh: "短信",
        path: "/bin/sms",
        color: 0x00_00_B9_6B,
        default_on: true,
    },
    App {
        key: "contacts",
        name: "Contacts",
        name_zh: "通讯录",
        path: "/bin/contacts",
        color: 0x00_9B_59_B6,
        default_on: true,
    },
    App {
        key: "settings",
        name: "Settings",
        name_zh: "设置",
        path: "/bin/settings",
        color: 0x00_00_B9_6B,
        default_on: true,
    },
    App {
        key: "files",
        name: "Files",
        name_zh: "文件",
        path: "/bin/files",
        color: 0x00_F1_C4_0F,
        default_on: true,
    },
    App {
        key: "browser",
        name: "Browser",
        name_zh: "浏览器",
        path: "/bin/browser",
        color: 0x00_F9_73_16,
        default_on: true,
    },
    App {
        key: "calc",
        name: "Calculator",
        name_zh: "计算器",
        path: "/bin/calculator",
        color: 0x00_EA_B3_08,
        default_on: true,
    },
    App {
        key: "calendar",
        name: "Calendar",
        name_zh: "日历",
        path: "/bin/calendar",
        color: 0x00_E7_4C_3C,
        default_on: false,
    },
    App {
        key: "camera",
        name: "Camera",
        name_zh: "相机",
        path: "/bin/camera",
        color: 0x00_34_49_5E,
        default_on: false,
    },
    App {
        key: "clock",
        name: "Clock",
        name_zh: "时钟",
        path: "/bin/clock",
        color: 0x00_5D_6D_7E,
        default_on: true,
    },
    App {
        key: "gallery",
        name: "Gallery",
        name_zh: "图库",
        path: "/bin/gallery",
        color: 0x00_AF_7A_C5,
        default_on: false,
    },
    App {
        key: "music",
        name: "Music",
        name_zh: "音乐",
        path: "/bin/music",
        color: 0x00_E6_7E_22,
        default_on: false,
    },
    App {
        key: "mail",
        name: "Mail",
        name_zh: "邮件",
        path: "/bin/mail",
        color: 0x00_29_80_B9,
        default_on: false,
    },
    App {
        key: "map",
        name: "Maps",
        name_zh: "地图",
        path: "/bin/map",
        color: 0x00_27_AE_60,
        default_on: false,
    },
    App {
        key: "weather",
        name: "Weather",
        name_zh: "天气",
        path: "/bin/weather",
        color: 0x00_5D_6D_7E,
        default_on: false,
    },
    App {
        key: "store",
        name: "Store",
        name_zh: "应用商店",
        path: "/bin/store",
        color: 0x00_7D_3C_98,
        default_on: true,
    },
    App {
        key: "terminal",
        name: "Terminal",
        name_zh: "终端",
        path: "/bin/terminal",
        color: 0x00_22_22_22,
        default_on: false,
    },
    App {
        key: "cloud",
        name: "Cloud",
        name_zh: "云服务",
        path: "/bin/cloud",
        color: 0x00_38_9F_D6,
        default_on: true,
    },
    App {
        key: "video",
        name: "Video",
        name_zh: "视频",
        path: "/bin/video",
        color: 0x00_E7_4C_3C,
        default_on: false,
    },
    App {
        key: "notes",
        name: "Notes",
        name_zh: "备忘录",
        path: "/bin/notes",
        color: 0x00_F1_C4_0F,
        default_on: false,
    },
    App {
        key: "lcapp",
        name: "LCApp",
        name_zh: "LCApp",
        path: "/bin/lcapp",
        color: 0x00_2E_CC_71,
        default_on: true,
    },
];

/// 过滤后的可见应用（桌面只显示 modules 中的、且不在 modules_removed 中的应用）。
static mut VISIBLE: [usize; 24] = [0; 24];
static mut VISIBLE_COUNT: usize = 0;

const MODULES_PATH: &str = "/data/oobe/modules";
const REMOVED_PATH: &str = "/data/oobe/modules_removed";

// ==================== 桌面模型（页 / 项 / 文件夹 / 小组件） ====================

const PAGE_COUNT: usize = 2;
const MAX_PAGE_ITEMS: usize = 24;
const FOLDER_MAX: usize = 9;
const FOLDER_ICON: i32 = 48;
const FOLDER_NAME_PX: f32 = 17.0;
const FOLDER_PANEL_W: usize = 396;
const FOLDER_PANEL_H: usize = 500;
const FOLDER_RADIUS: i32 = 24;

/// Dock 固定应用：电话、短信、浏览器、相机、设置。
const DOCK_KEYS: [&str; 5] = ["phone", "sms", "browser", "camera", "settings"];
/// 系统工具文件夹内容。
const FOLDER_KEYS: [&str; 6] = ["calculator", "clock", "files", "terminal", "cloud", "mail"];
/// 第一屏常用应用；第二屏其他应用。
const PAGE0_APPS: [&str; 6] = [
    "contacts", "calendar", "gallery", "music", "weather", "lcapp",
];
const PAGE1_APPS: [&str; 4] = ["map", "store", "video", "notes"];

/// 小组件数据文件（缺失时使用默认值）。
const WEATHER_TEMP_PATH: &str = "/data/oobe/weather_temp";
const WEATHER_COND_PATH: &str = "/data/oobe/weather_cond";
const NEXT_EVENT_PATH: &str = "/data/oobe/next_event";
const BATTERY_PATH: &str = "/data/oobe/battery";
const CHARGING_PATH: &str = "/data/oobe/charging";
const GRID_PATH: &str = "/data/oobe/grid_cols";
const FOLDER_NAME_PATH: &str = "/data/oobe/folder_name";

#[derive(Clone, Copy, PartialEq)]
enum ItemKind {
    App(usize),
    Folder,
    Widget(WidgetKind),
}

#[derive(Clone, Copy, PartialEq)]
enum WidgetKind {
    Clock,
    Weather,
    Calendar,
    Gallery,
    Battery,
}

#[derive(Clone, Copy)]
struct Item {
    kind: ItemKind,
}

#[derive(Clone, Copy)]
struct PageItems {
    count: usize,
    items: [Item; MAX_PAGE_ITEMS],
}

static mut PAGES: [PageItems; PAGE_COUNT] = [PageItems {
    count: 0,
    items: [Item {
        kind: ItemKind::App(0),
    }; MAX_PAGE_ITEMS],
}; PAGE_COUNT];

/// 放置结果（自动布局：按项目顺序行优先填入第一个可容纳的空位）。
#[derive(Clone, Copy)]
struct PlacedItem {
    index: usize,
    col: i32,
    row: i32,
    w: i32,
    h: i32,
    kind: ItemKind,
}

impl PlacedItem {
    const fn zero() -> PlacedItem {
        PlacedItem {
            index: 0,
            col: 0,
            row: 0,
            w: 0,
            h: 0,
            kind: ItemKind::App(0),
        }
    }
}

static mut PLACED: [PlacedItem; MAX_PAGE_ITEMS] = [PlacedItem::zero(); MAX_PAGE_ITEMS];
static mut PLACED_COUNT: usize = 0;

/// Dock 固定应用（按 DOCK_KEYS 解析，尊重 modules_removed）。
static mut DOCK_APPS: [usize; 5] = [usize::MAX; 5];
static mut DOCK_COUNT: usize = 0;

/// 系统工具文件夹内容。
static mut FOLDER_APPS: [usize; FOLDER_MAX] = [usize::MAX; FOLDER_MAX];
static mut FOLDER_COUNT: usize = 0;

// ==================== 8x8 位图字体 ====================

static FONT_DIGITS: [[u8; 8]; 10] = [
    // 0
    [0x3C, 0x42, 0x46, 0x4A, 0x52, 0x62, 0x3C, 0x00],
    // 1
    [0x08, 0x18, 0x28, 0x08, 0x08, 0x08, 0x3E, 0x00],
    // 2
    [0x3C, 0x42, 0x02, 0x0C, 0x30, 0x40, 0x7E, 0x00],
    // 3
    [0x3C, 0x42, 0x02, 0x1C, 0x02, 0x42, 0x3C, 0x00],
    // 4
    [0x0C, 0x14, 0x24, 0x44, 0x7E, 0x04, 0x04, 0x00],
    // 5
    [0x7E, 0x40, 0x7C, 0x02, 0x02, 0x42, 0x3C, 0x00],
    // 6
    [0x3C, 0x40, 0x40, 0x7C, 0x42, 0x42, 0x3C, 0x00],
    // 7
    [0x7E, 0x02, 0x04, 0x08, 0x10, 0x10, 0x10, 0x00],
    // 8
    [0x3C, 0x42, 0x42, 0x3C, 0x42, 0x42, 0x3C, 0x00],
    // 9
    [0x3C, 0x42, 0x42, 0x3E, 0x02, 0x02, 0x3C, 0x00],
];

static FONT_COLON: [u8; 8] = [0x00, 0x18, 0x18, 0x00, 0x00, 0x18, 0x18, 0x00];

static FONT_DASH: [u8; 8] = [0x00, 0x00, 0x00, 0x00, 0x7E, 0x00, 0x00, 0x00];

static FONT_SPACE: [u8; 8] = [0x00; 8];

static FONT_UPPER: [[u8; 8]; 26] = [
    // A
    [0x18, 0x24, 0x42, 0x42, 0x7E, 0x42, 0x42, 0x00],
    // B
    [0x7C, 0x42, 0x42, 0x7C, 0x42, 0x42, 0x7C, 0x00],
    // C
    [0x3C, 0x42, 0x40, 0x40, 0x40, 0x42, 0x3C, 0x00],
    // D
    [0x78, 0x44, 0x42, 0x42, 0x42, 0x44, 0x78, 0x00],
    // E
    [0x7E, 0x40, 0x40, 0x7C, 0x40, 0x40, 0x7E, 0x00],
    // F
    [0x7E, 0x40, 0x40, 0x7C, 0x40, 0x40, 0x40, 0x00],
    // G
    [0x3C, 0x42, 0x40, 0x4E, 0x42, 0x42, 0x3C, 0x00],
    // H
    [0x42, 0x42, 0x42, 0x7E, 0x42, 0x42, 0x42, 0x00],
    // I
    [0x3E, 0x08, 0x08, 0x08, 0x08, 0x08, 0x3E, 0x00],
    // J
    [0x1E, 0x04, 0x04, 0x04, 0x04, 0x44, 0x38, 0x00],
    // K
    [0x42, 0x44, 0x48, 0x70, 0x48, 0x44, 0x42, 0x00],
    // L
    [0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x7E, 0x00],
    // M
    [0x42, 0x66, 0x5A, 0x42, 0x42, 0x42, 0x42, 0x00],
    // N
    [0x42, 0x62, 0x52, 0x4A, 0x46, 0x42, 0x42, 0x00],
    // O
    [0x3C, 0x42, 0x42, 0x42, 0x42, 0x42, 0x3C, 0x00],
    // P
    [0x7C, 0x42, 0x42, 0x7C, 0x40, 0x40, 0x40, 0x00],
    // Q
    [0x3C, 0x42, 0x42, 0x42, 0x4A, 0x44, 0x3A, 0x00],
    // R
    [0x7C, 0x42, 0x42, 0x7C, 0x48, 0x44, 0x42, 0x00],
    // S
    [0x3C, 0x42, 0x40, 0x3C, 0x02, 0x42, 0x3C, 0x00],
    // T
    [0x7F, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x00],
    // U
    [0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x3C, 0x00],
    // V
    [0x42, 0x42, 0x42, 0x42, 0x42, 0x24, 0x18, 0x00],
    // W
    [0x42, 0x42, 0x42, 0x42, 0x5A, 0x66, 0x42, 0x00],
    // X
    [0x42, 0x42, 0x24, 0x18, 0x24, 0x42, 0x42, 0x00],
    // Y
    [0x41, 0x22, 0x14, 0x08, 0x08, 0x08, 0x08, 0x00],
    // Z
    [0x7E, 0x02, 0x04, 0x08, 0x10, 0x20, 0x7E, 0x00],
];

static FONT_LOWER: [[u8; 8]; 26] = [
    // a
    [0x00, 0x00, 0x3C, 0x02, 0x3E, 0x42, 0x3E, 0x00],
    // b
    [0x40, 0x40, 0x7C, 0x42, 0x42, 0x42, 0x7C, 0x00],
    // c
    [0x00, 0x00, 0x3C, 0x42, 0x40, 0x42, 0x3C, 0x00],
    // d
    [0x02, 0x02, 0x3E, 0x42, 0x42, 0x42, 0x3E, 0x00],
    // e
    [0x00, 0x00, 0x3C, 0x42, 0x7E, 0x40, 0x3C, 0x00],
    // f
    [0x0E, 0x10, 0x10, 0x3C, 0x10, 0x10, 0x10, 0x00],
    // g
    [0x00, 0x00, 0x3E, 0x42, 0x42, 0x3E, 0x02, 0x3C],
    // h
    [0x40, 0x40, 0x7C, 0x42, 0x42, 0x42, 0x42, 0x00],
    // i
    [0x08, 0x00, 0x18, 0x08, 0x08, 0x08, 0x3E, 0x00],
    // j
    [0x04, 0x00, 0x0C, 0x04, 0x04, 0x44, 0x38, 0x00],
    // k
    [0x40, 0x40, 0x44, 0x48, 0x70, 0x48, 0x44, 0x00],
    // l
    [0x18, 0x08, 0x08, 0x08, 0x08, 0x08, 0x3E, 0x00],
    // m
    [0x00, 0x00, 0x66, 0x5A, 0x5A, 0x42, 0x42, 0x00],
    // n
    [0x00, 0x00, 0x7C, 0x42, 0x42, 0x42, 0x42, 0x00],
    // o
    [0x00, 0x00, 0x3C, 0x42, 0x42, 0x42, 0x3C, 0x00],
    // p
    [0x00, 0x00, 0x7C, 0x42, 0x42, 0x7C, 0x40, 0x40],
    // q
    [0x00, 0x00, 0x3E, 0x42, 0x42, 0x3E, 0x02, 0x02],
    // r
    [0x00, 0x00, 0x5C, 0x62, 0x40, 0x40, 0x40, 0x00],
    // s
    [0x00, 0x00, 0x3E, 0x40, 0x3C, 0x02, 0x7C, 0x00],
    // t
    [0x10, 0x10, 0x7C, 0x10, 0x10, 0x10, 0x0E, 0x00],
    // u
    [0x00, 0x00, 0x42, 0x42, 0x42, 0x42, 0x3E, 0x00],
    // v
    [0x00, 0x00, 0x42, 0x42, 0x42, 0x24, 0x18, 0x00],
    // w
    [0x00, 0x00, 0x42, 0x42, 0x5A, 0x5A, 0x24, 0x00],
    // x
    [0x00, 0x00, 0x42, 0x24, 0x18, 0x24, 0x42, 0x00],
    // y
    [0x00, 0x00, 0x42, 0x42, 0x42, 0x3E, 0x02, 0x3C],
    // z
    [0x00, 0x00, 0x7E, 0x04, 0x18, 0x20, 0x7E, 0x00],
];

fn glyph(c: u8) -> &'static [u8; 8] {
    match c {
        b'0'..=b'9' => &FONT_DIGITS[(c - b'0') as usize],
        b':' => &FONT_COLON,
        b'-' => &FONT_DASH,
        b'A'..=b'Z' => &FONT_UPPER[(c - b'A') as usize],
        b'a'..=b'z' => &FONT_LOWER[(c - b'a') as usize],
        _ => &FONT_SPACE, // 未知字符渲染为空
    }
}

// ==================== 帧缓冲与状态 ====================

/// 映射到用户空间的帧缓冲指针（由 `fb_init` 经 open/mmap /dev/fb0 建立）。
static mut FB: *mut u32 = core::ptr::null_mut();
/// 实际分辨率（默认 540x960，可被 ioctl 结果覆盖）。
static mut FB_W: usize = DISPLAY_W;
static mut FB_H: usize = DISPLAY_H;
/// 每行像素数（stride / 4）。
static mut FB_STRIDE_PX: usize = DISPLAY_W;

// ---- 脏区域追踪（应用直绘 → 内核脏区域合成器） ----
/// 保持打开的 /dev/fb0 fd（FBIO_PRESENT 提交脏区用）。
static mut FB_FD: isize = -1;
/// 本帧累计的脏矩形（mark_dirty 合并；超限置 FULL）。
static mut DIRTY_FULL: bool = false;
static mut DIRTY_COUNT: usize = 0;
static mut DIRTY: [(i32, i32, u32, u32); MAX_DIRTY] = [(0, 0, 0, 0); MAX_DIRTY];
/// FBIO_PRESENT 用户缓冲区（count + rects，与内核 devfs 约定布局一致）。
static mut DAMAGE_BUF: [u8; 4 + MAX_DIRTY * 16] = [0u8; 4 + MAX_DIRTY * 16];

/// 标记一个变更矩形（合并重叠；超限退化为整屏）。重绘到 BACK_BUF 后、
/// `flip` 前对每个变更区域调用；`flip` 只把这些矩形拷到显存并提交内核。
fn mark_dirty(x: i32, y: i32, w: u32, h: u32) {
    unsafe {
        if w == 0 || h == 0 || DIRTY_FULL {
            return;
        }
        for i in 0..DIRTY_COUNT {
            let d = DIRTY[i];
            if x < d.0 + d.2 as i32
                && d.0 < x + w as i32
                && y < d.1 + d.3 as i32
                && d.1 < y + h as i32
            {
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
        DIRTY[DIRTY_COUNT] = (x, y, w, h);
        DIRTY_COUNT += 1;
    }
}

/// 提交脏区并请求呈现（FBIO_PRESENT）：dirty 列表编码进 DAMAGE_BUF 后 ioctl。
fn fb_submit_present() {
    unsafe {
        if FB_FD < 0 || FB.is_null() {
            DIRTY_FULL = false;
            DIRTY_COUNT = 0;
            return;
        }
        if DIRTY_FULL || DIRTY_COUNT == 0 {
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
        syscall3(
            SYS_IOCTL,
            FB_FD as usize,
            FBIO_PRESENT,
            DAMAGE_BUF.as_ptr() as usize,
        );
        DIRTY_FULL = false;
        DIRTY_COUNT = 0;
    }
}

/// 双缓冲 back buffer：所有绘制先写入这里，`flip()` 一次性拷贝到显存，
/// 避免逐像素直写显存导致的撕裂与闪烁。
static mut BACK_BUF: [u32; DISPLAY_W * DISPLAY_H] = [0u32; DISPLAY_W * DISPLAY_H];
/// 是否需要整屏重绘（初始为 true；输入 / 窗口变化 / 手势动画时置位）。
static mut NEEDS_REDRAW: bool = true;
/// 仅状态栏失效：分钟变化时只重绘 24px 状态栏行（局部脏矩形 + flip），
/// 避免每分钟整屏重绘（背景渐变波 + 24 图标 + TTF 标签 + 毛玻璃全部重做）。
static mut STATUS_DIRTY: bool = false;
/// 上次绘制到状态栏的分钟数（用于时钟变化时触发重绘）。
static mut LAST_MINUTE: u64 = u64::MAX;

/// 来电/通话横幅（来自 rild.state）：0 = 无，1 = 来电（可点击接听），2 = 通话中。
static mut CALL_BANNER: u8 = 0;
static mut CALL_BANNER_NUM: [u8; 24] = [0; 24];
static mut CALL_BANNER_LEN: usize = 0;
static mut CALL_BANNER_POLL: u64 = 0;
/// 横幅入场起始时刻（Fluid Cloud 式从状态栏后方弹入；0 = 已入场完毕）。
static mut BANNER_T0: u64 = 0;
/// 横幅退场起始时刻（上滑收回状态栏后方 + 淡出；0 = 无退场动画）。
static mut BANNER_EXIT_T0: u64 = 0;
/// 横幅入场 260ms FastOutSlowIn / 退场 220ms EaseIn。
const BANNER_IN_US: u64 = 260_000;
const BANNER_OUT_US: u64 = 220_000;
const BANNER_H: i32 = 42;
const BANNER_Y: i32 = 34;

/// 横幅入/退场动画是否进行中（主循环保持重绘判定）。
fn banner_anim_active() -> bool {
    unsafe {
        let now = now_us();
        (CALL_BANNER != 0 && BANNER_T0 != 0 && now.saturating_sub(BANNER_T0) < BANNER_IN_US)
            || (BANNER_EXIT_T0 != 0 && now.saturating_sub(BANNER_EXIT_T0) < BANNER_OUT_US)
    }
}

/// 横幅内容与宽度（draw 与 hit 共用，避免两处文本拼装漂移）。
fn call_banner_layout() -> ([u8; 44], usize, i32) {
    unsafe {
        let num = &CALL_BANNER_NUM[..CALL_BANNER_LEN];
        let mut s = [0u8; 44];
        let (prefix, plen) = if CALL_BANNER == 1 {
            ("来电 ".as_bytes(), 6usize)
        } else {
            ("通话中 ".as_bytes(), 7usize)
        };
        s[..plen].copy_from_slice(prefix);
        let nl = num.len().min(36);
        s[plen..plen + nl].copy_from_slice(&num[..nl]);
        let total = plen + nl;
        let w = text_width(&s[..total]) + 48;
        (s, total, w)
    }
}

/// 在指定高度以指定透明度绘制横幅玻璃药丸（Fluid Cloud 胶囊语义）。
fn draw_banner_at(y: i32, alpha: u32) {
    unsafe {
        let (s, total, w) = call_banner_layout();
        let x = (DISPLAY_W as i32 - w) / 2;
        // 来电 = 强调绿玻璃（白字）；"通话中" = 浅色磨砂玻璃（深字）。
        let style = if CALL_BANNER == 1 {
            &STYLE_ACCENT
        } else {
            &GLASS_FROST
        };
        let text_col = if CALL_BANNER == 1 {
            COLOR_TEXT
        } else {
            COLOR_TEXT_MAIN
        };
        // 临时降低全局像素透明度实现整体淡入淡出（Toast 同款机制）。
        let prev = PIX_ALPHA;
        PIX_ALPHA = alpha.min(255);
        draw_glass_style(x, y, w, BANNER_H, 16, style);
        draw_text(x + 20, y + 14, &s[..total], text_col);
        if CALL_BANNER == 1 {
            draw_text(x + w - 64, y + 14, "接听".as_bytes(), COLOR_TEXT);
        }
        PIX_ALPHA = prev;
    }
}

/// 通话横幅绘制（桌面态，状态栏下方）。
///
/// 动效（ColorOS 15 流体云胶囊）：出现时从状态栏后方弹入（260ms
/// FastOutSlowIn 下滑 + 淡入），通话结束时上滑收回状态栏后方并淡出
/// （220ms EaseIn）——替代原先的瞬现/瞬消。
fn draw_call_banner() {
    unsafe {
        let now = now_us();
        if BANNER_EXIT_T0 != 0 {
            let t = now.saturating_sub(BANNER_EXIT_T0) as f32 / BANNER_OUT_US as f32;
            if t >= 1.0 {
                BANNER_EXIT_T0 = 0;
                return;
            }
            let e = Easing::EaseIn.sample(t);
            let y = BANNER_Y - (((BANNER_Y + BANNER_H + 6) as f32) * e) as i32;
            draw_banner_at(y, (255.0 * (1.0 - e)) as u32);
            return;
        }
        if CALL_BANNER == 0 {
            return;
        }
        let tin = if BANNER_T0 == 0 {
            1.0
        } else {
            (now.saturating_sub(BANNER_T0) as f32 / BANNER_IN_US as f32).min(1.0)
        };
        let e = Easing::FastOutSlowIn.sample(tin);
        let y = BANNER_Y - ((BANNER_Y + 12) as f32 * (1.0 - e)) as i32;
        draw_banner_at(y, (255.0 * e) as u32);
    }
}

/// 桌面与系统面板状态。全部采用定长数据，保持 no_std/无堆。
static mut CURRENT_PAGE: usize = 0;

// ==================== 桌面翻页动画（对标 ColorOS 15 跟手翻页） ====================
//
// PAGE_OFFSET 为当前页相对原位的水平像素偏移（右滑为正，px）：
//   - 跟手段：手指横向拖动直接驱动 PAGE_OFFSET（页界外 0.35x 橡皮筋阻尼）；
//   - 吸附段：松手后 PAGE_SPRING 把偏移弹回 0（翻页时先提交 CURRENT_PAGE，
//     新当前页从 ±屏宽 弹入原位，视觉上内容连续滑动）。
static mut PAGE_OFFSET: f32 = 0.0;
static mut PAGE_SPRING: Spring = Spring::fixed();
static mut PAGE_SPRING_T: u64 = 0;
/// 吸附弹簧动画进行中。
static mut PAGE_ANIM: bool = false;
/// 跟手翻页进行中（手指未抬起）。
static mut PAGE_DRAG: bool = false;
/// 本轮触摸已进入翻页追踪（越过阈值后整轮手势保持翻页语义）。
static mut PAGE_TRACK: bool = false;

// ==================== 启动/恢复转场锚点（最近任务卡片起点） ====================
//
// 从最近任务卡片点开应用时，启动/恢复放大转场以卡片中心为起点（ColorOS 15 /
// 鸿蒙：应用从所点卡片放大进入，而非桌面图标原位）。一次性消费：被
// start_launch / resume_app_at 取走后自动清空，其余路径不受影响。
static mut LAUNCH_ANCHOR: Option<(i32, i32)> = None;

/// 设置下一次启动/恢复转场的放大起点（屏幕像素坐标）。
pub fn set_launch_anchor(x: i32, y: i32) {
    unsafe { LAUNCH_ANCHOR = Some((x, y)) }
}

/// 取走锚点（一次性；None = 用图标原位）。
pub fn take_launch_anchor() -> Option<(i32, i32)> {
    unsafe {
        let a = LAUNCH_ANCHOR;
        LAUNCH_ANCHOR = None;
        a
    }
}

static mut THEME_DARK: bool = false;
static mut WALLPAPER: usize = 2;
static mut LANG_ZH: bool = true;
static mut WIFI_ENABLED: bool = true;
static mut BRIGHTNESS: u32 = 100;
/// 亮度查找表：`LUT[v] = v * level / 100`。亮度变化时重建，避免 flip()
/// 每帧对每像素做 3 次乘除（默认亮度 85 时这是每帧隐藏的大开销）。
static mut BRIGHTNESS_LUT: [u32; 256] = [0; 256];
/// 已生成 LUT 的亮度级别（u32::MAX = 尚未生成，首次 flip 时自动重建）。
static mut BRIGHTNESS_LUT_LEVEL: u32 = u32::MAX;
static mut SHADE_OPEN: bool = false;
static mut EDIT_MODE: bool = false;
static mut EDIT_APP: usize = usize::MAX;
static mut SHOW_BATTERY_PERCENT: bool = false;
static mut SOUND_ENABLED: bool = true;
static mut RECENT_APP: usize = usize::MAX;
/// 最近任务面板是否打开（应用以退出码 2 退出或点击导航按钮时置位）。
static mut RECENTS_OPEN: bool = false;
/// 最近任务卡片列表（打开时重建，最多 6 个）。
static mut RECENTS_LIST: [usize; 6] = [usize::MAX; 6];
static mut RECENTS_COUNT: usize = 0;

// ==================== 最近任务动效（对标 ColorOS 15 错峰入场 / 上滑清理） ====================
//
//   - 打开：卡片按槽位错峰 30ms 上滑入场（200ms EaseOut，位移 24px）；
//   - 关闭：整体 150ms 下滑 12px + 压暗层淡出；
//   - 单卡清理：按住卡片上拖跟手，松手越过阈值后 200ms 上飞出屏，
//     其余卡片 220ms EaseOut 补位到新格位（x/y 同时插值）。
static mut RECENTS_T0: u64 = 0;
/// 关闭动画进行中（完成后才真正置 RECENTS_OPEN = false）。
static mut RECENTS_CLOSING: bool = false;
static mut RECENTS_CLOSE_T0: u64 = 0;
/// 跟手清理中的卡槽（usize::MAX = 无）与当前跟手位移（px，通常 ≤ 0）。
static mut REC_DRAG_SLOT: usize = usize::MAX;
static mut REC_DRAG_Y: i32 = 0;
/// 上飞中的卡（已从列表移除）：应用索引、起飞位置与动画起点。
static mut REC_FLY_APP: usize = usize::MAX;
static mut REC_FLY_X: i32 = 0;
static mut REC_FLY_Y: i32 = 0;
static mut REC_FLY_T0: u64 = 0;
/// 未达阈值的回弹（从 REC_BACK_FROM 回到 0，200ms EaseOut）。
static mut REC_BACK: bool = false;
static mut REC_BACK_T0: u64 = 0;
static mut REC_BACK_FROM: i32 = 0;
static mut REC_BACK_SLOT: usize = usize::MAX;
/// 补位动画：移动前各卡位置（x, y）与动画起点；被移除的旧槽位
/// （新槽位 i 的旧位置 = REC_SHIFT_FROM[i]（i < 移除槽）或 [i+1]）。
static mut REC_SHIFT_T0: u64 = 0;
static mut REC_SHIFT_FROM: [(i32, i32); 6] = [(0, 0); 6];
static mut REC_REMOVED_SLOT: usize = usize::MAX;

/// Toast 与触摸涟漪均由主循环按时间自动失效。
static mut TOAST: &'static [u8] = b"";
static mut TOAST_UNTIL: u64 = 0;
/// Toast 出现时刻（入场动画 180ms：底部上滑 16px + 淡入）。
static mut TOAST_T0: u64 = 0;
static mut RIPPLE_X: i32 = 0;
static mut RIPPLE_Y: i32 = 0;
static mut RIPPLE_START: u64 = 0;
static mut RIPPLE_ACTIVE: bool = false;

/// 网格列数（默认 4，编辑模式可切换 5）。
static mut GRID_COLS: i32 = GRID_COLS_DEFAULT;
/// 时钟小组件秒级刷新 / 相册轮播。
static mut LAST_SECOND: u64 = u64::MAX;
static mut GALLERY_TICK: usize = 0;

/// 文件夹状态。
#[derive(Clone, Copy, PartialEq)]
enum FolderPhase {
    Closed,
    Opening,
    Open,
    Closing,
}
static mut FOLDER_PHASE: FolderPhase = FolderPhase::Closed;
static mut FOLDER_ANIM: Spring = Spring::fixed();
static mut FOLDER_LAST_T: u64 = 0;
static mut FOLDER_ANCHOR_X: i32 = 0;
static mut FOLDER_ANCHOR_Y: i32 = 0;
static mut FOLDER_TILE_DIRTY: bool = false;
/// 文件夹关闭动画结束后要启动的应用（从文件夹内点击时设置）。
static mut FOLDER_LAUNCH: usize = usize::MAX;

/// 文件夹重命名面板。
static mut RENAME_OPEN: bool = false;
static mut RENAME_BUF: [u8; 24] = [0; 24];
static mut RENAME_LEN: usize = 0;
static mut FOLDER_NAME: [u8; 24] = [0; 24];
static mut FOLDER_NAME_LEN: usize = 0;

/// 编辑模式拖拽。
static mut DRAG_ACTIVE: bool = false;
static mut DRAG_MOVED: bool = false;
static mut DRAG_ITEM: usize = usize::MAX;
static mut DRAG_APP: usize = usize::MAX;
static mut DRAG_X: i32 = 0;
static mut DRAG_Y: i32 = 0;

/// 鼠标长按（进入编辑模式）。
static mut MOUSE_PRESS_US: u64 = 0;
static mut MOUSE_LONG: bool = false;

// ==================== 锁屏 ====================
//
// OOBE 已保存安全配置（security_type / security_hash / duress_hash /
// duress_action，见 apps/oobe save_security），但此前没有任何模块消费它。
// 本模块实现系统锁屏：启动时按安全配置自动上锁、快捷面板「锁屏」按钮手动
// 上锁、云服务远程锁定（/data/oobe/locked=1）即时上锁；解锁支持滑动
// （无安全）、PIN、3x3 图案、密码四种方式，全部与 OOBE 的 FNV-1a 哈希比对。

const SECURITY_TYPE_PATH: &str = "/data/oobe/security_type";
const SECURITY_HASH_PATH: &str = "/data/oobe/security_hash";
const DURESS_HASH_PATH: &str = "/data/oobe/duress_hash";
const DURESS_ACTION_PATH: &str = "/data/oobe/duress_action";
const LOCKED_PATH: &str = "/data/oobe/locked";

/// 锁屏安全类型：0=无（滑动解锁），1=PIN，2=图案，3=密码。
static mut LOCK_SEC_TYPE: u8 = 0;
/// 系统是否处于锁屏状态。
static mut LOCKED: bool = false;

// ==================== 锁屏转场动画（对标 ColorOS 15 上滑解锁） ====================
//
// 锁屏整层（渐变底 + 内容）作为一个「层」平移：解锁时从当前位置上滑出屏
// （-屏高），露出下方桌面；锁定时反向下滑入位。滑动解锁支持跟手：
// 手指拖动直接驱动 LOCK_LAYER_OFF，松手按位移/速度结算。
#[derive(Clone, Copy, PartialEq)]
enum LockPhase {
    /// 静止（锁定态静态渲染 / 已解锁）。
    None,
    /// 解锁中：锁屏层上滑出屏，完成后 LOCKED = false。
    Unlocking,
    /// 锁定中：锁屏层从 -屏高 下滑入位，完成后接管渲染。
    Locking,
    /// 上滑跟手未达阈值的回弹（回 0）。
    Rebound,
}
static mut LOCK_PHASE: LockPhase = LockPhase::None;
static mut LOCK_T0: u64 = 0;
/// 锁屏层当前垂直偏移（px，≤ 0；0 = 完整覆盖屏幕）。
static mut LOCK_LAYER_OFF: f32 = 0.0;
/// 转场起点偏移（Unlocking 从跟手位置起滑 / Rebound 起点）。
static mut LOCK_FROM: f32 = 0.0;
/// 滑动解锁跟手进行中。
static mut LOCK_DRAG: bool = false;
/// 胁迫动作：0=fake_system（伪解锁），1=wipe（清除数据），2=alert（提示上报）。
static mut LOCK_DURESS_ACTION: u8 = 0;
/// 已保存的 security_hash / duress_hash（8 字节小写 ASCII hex）。
static mut SEC_HASH: [u8; 8] = [0; 8];
static mut DURESS_HASH: [u8; 8] = [0; 8];
/// 已输入的 PIN/密码（最多 16 位）。
static mut LOCK_INPUT: [u8; 16] = [0; 16];
static mut LOCK_INPUT_LEN: usize = 0;
/// 图案模式已选中的点序列（1..=9）。
static mut LOCK_PATTERN: [u8; 9] = [0; 9];
static mut LOCK_PATTERN_LEN: usize = 0;
/// 解锁失败提示剩余重绘帧数（>0 时锁屏界面显示红字提示）。
static mut LOCK_FAIL_TICKS: u32 = 0;
/// 锁屏时钟分钟（分钟变化时重绘）。
static mut LOCK_MINUTE: u64 = u64::MAX;

// ==================== 系统调用封装 ====================

#[inline(always)]
unsafe fn syscall1(n: usize, a1: usize) -> usize {
    let ret: usize;
    asm!("svc #0", in("x8") n, inlateout("x0") a1 => ret, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall2(n: usize, a1: usize, a2: usize) -> usize {
    let ret: usize;
    asm!(
        "svc #0",
        in("x8") n,
        inlateout("x0") a1 => ret,
        in("x1") a2,
        options(nostack)
    );
    ret
}

#[inline(always)]
unsafe fn syscall3(n: usize, a1: usize, a2: usize, a3: usize) -> usize {
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

#[inline(always)]
unsafe fn syscall4(n: usize, a1: usize, a2: usize, a3: usize, a4: usize) -> usize {
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
unsafe fn syscall5(n: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> usize {
    let ret: usize;
    asm!(
        "svc #0",
        in("x8") n,
        inlateout("x0") a1 => ret,
        in("x1") a2,
        in("x2") a3,
        in("x3") a4,
        in("x4") a5,
        options(nostack)
    );
    ret
}

#[inline(always)]
unsafe fn syscall6(
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

unsafe fn write_fd(fd: usize, buf: &[u8]) -> usize {
    syscall3(SYS_WRITE, fd, buf.as_ptr() as usize, buf.len())
}

fn print(s: &str) {
    unsafe {
        write_fd(1, s.as_bytes());
    }
}

fn print_dec(mut v: u32) {
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
    unsafe {
        write_fd(1, &buf[i..]);
    }
}

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

unsafe fn exit(code: usize) -> ! {
    syscall1(SYS_EXIT, code);
    loop {}
}

/// 当前时间（微秒，自启动）。
fn now_us() -> u64 {
    let mut tv = [0u8; 16];
    unsafe {
        syscall2(SYS_GETTIMEOFDAY, tv.as_mut_ptr() as usize, 0);
        let sec = u64::from_le_bytes([tv[0], tv[1], tv[2], tv[3], tv[4], tv[5], tv[6], tv[7]]);
        let usec =
            u64::from_le_bytes([tv[8], tv[9], tv[10], tv[11], tv[12], tv[13], tv[14], tv[15]]);
        sec * 1_000_000 + usec
    }
}

// ==================== 文件操作 ====================

static mut PATH_BUF: [u8; 128] = [0u8; 128];

unsafe fn open(path: &str, flags: usize) -> isize {
    let n = path.len().min(PATH_BUF.len() - 1);
    PATH_BUF[..n].copy_from_slice(&path.as_bytes()[..n]);
    PATH_BUF[n] = 0;
    syscall3(SYS_OPENAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, flags) as isize
}

// ==================== 模块过滤 ====================

/// 读取整个文件到 buf，返回长度；失败返回 -1。
unsafe fn read_file(path: &str, buf: &mut [u8]) -> isize {
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

/// 轮询 rild.state 的来电/通话状态（每 200ms），变化时置重绘。
fn poll_rild_banner() {
    unsafe {
        if CALL_BANNER_POLL != 0 && now_us().saturating_sub(CALL_BANNER_POLL) < 200_000 {
            return;
        }
        CALL_BANNER_POLL = now_us();
    }
    let mut buf = [0u8; 384];
    let n = unsafe { read_file("/var/run/rild.state", &mut buf) };
    if n <= 0 {
        unsafe {
            // 横幅在屏 → 播放退场（上滑收回 + 淡出）而非瞬消。
            if CALL_BANNER != 0 {
                BANNER_EXIT_T0 = now_us();
            }
            CALL_BANNER = 0;
        }
        return;
    }
    let n = n as usize;
    let mut call: u8 = 0;
    let mut num = [0u8; 24];
    let mut num_len = 0usize;
    let mut start = 0usize;
    while start < n {
        let end = match buf[start..n].iter().position(|&b| b == b'\n') {
            Some(x) => start + x,
            None => n,
        };
        let line = &buf[start..end];
        if line.starts_with(b"CALL:") {
            let v = &line[5..];
            call = if v == b"RINGING" {
                1
            } else if v == b"ACTIVE" {
                2
            } else {
                0
            };
        } else if line.starts_with(b"NUM:") {
            num_len = (line.len() - 4).min(num.len());
            num[..num_len].copy_from_slice(&line[4..4 + num_len]);
        }
        start = end + 1;
    }
    unsafe {
        let changed = call != CALL_BANNER
            || (call != 0
                && (num_len != CALL_BANNER_LEN
                    || num[..num_len] != CALL_BANNER_NUM[..CALL_BANNER_LEN]));
        if changed {
            // 出现（含号码变化）→ 重播入场；消失 → 退场动画。
            if call != 0 {
                if CALL_BANNER == 0 {
                    BANNER_T0 = now_us();
                }
                BANNER_EXIT_T0 = 0;
            } else if CALL_BANNER != 0 {
                BANNER_EXIT_T0 = now_us();
            }
            CALL_BANNER = call;
            CALL_BANNER_NUM = num;
            CALL_BANNER_LEN = num_len;
            NEEDS_REDRAW = true;
        }
    }
}

/// 通话横幅点击区域（退场动画期间不可点击）。
fn call_banner_hit(x: i32, y: i32) -> bool {
    unsafe {
        if CALL_BANNER == 0 || BANNER_EXIT_T0 != 0 {
            return false;
        }
        let (_s, _total, w) = call_banner_layout();
        let bx = (DISPLAY_W as i32 - w) / 2;
        x >= bx && x < bx + w && y >= BANNER_Y && y < BANNER_Y + BANNER_H
    }
}

unsafe fn write_file(path: &str, data: &[u8]) -> bool {
    let fd = open(path, O_WRONLY | O_CREAT | O_TRUNC);
    if fd < 0 {
        return false;
    }
    let n = write_fd(fd as usize, data);
    syscall1(SYS_CLOSE, fd as usize);
    n == data.len()
}

fn bytes_contains(data: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || data.len() < needle.len() {
        return false;
    }
    for i in 0..=data.len() - needle.len() {
        if data[i..i + needle.len()]
            .iter()
            .zip(needle)
            .all(|(a, b)| a == b)
        {
            return true;
        }
    }
    false
}

/// 小组件数值的 1s TTL 缓存：第二页动画（翻页/最近任务/文件夹）与锁屏
/// 充电动效期间小组件每帧重绘，此前每帧 2-3 次 open/read/close。
static mut WGT_CACHE_VAL: [u32; 4] = [0; 4];
static mut WGT_CACHE_EXP: [u64; 4] = [0; 4];

fn read_number_cached(slot: usize, path: &str, fallback: u32) -> u32 {
    unsafe {
        let now = now_us();
        if now < WGT_CACHE_EXP[slot] {
            return WGT_CACHE_VAL[slot];
        }
        let v = read_number(path, fallback);
        WGT_CACHE_VAL[slot] = v;
        WGT_CACHE_EXP[slot] = now + 1_000_000;
        v
    }
}

fn read_number(path: &str, fallback: u32) -> u32 {
    let mut buf = [0u8; 8];
    let n = unsafe { read_file(path, &mut buf) };
    if n <= 0 {
        return fallback;
    }
    let mut value = 0u32;
    let mut seen = false;
    for &c in &buf[..n as usize] {
        if c.is_ascii_digit() {
            value = value.saturating_mul(10).saturating_add((c - b'0') as u32);
            seen = true;
        } else if seen {
            break;
        }
    }
    if seen {
        value
    } else {
        fallback
    }
}

fn save_number(path: &str, value: u32) {
    // 12 字节覆盖 u32 全量程（此前 [u8;4] 靠调用方值域约定，>=10000 即回绕越界）
    let mut buf = [0u8; 12];
    let mut pos = buf.len();
    let mut v = value;
    if v == 0 {
        pos -= 1;
        buf[pos] = b'0';
    }
    while v > 0 {
        pos -= 1;
        buf[pos] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    // 写失败时给出提示（此前静默假成功："无线已开启/亮度已更新"仍会弹出）。
    let ok = unsafe { write_file(path, &buf[pos..]) };
    if !ok {
        print("[launcher] 保存编号写入失败：");
        print(path);
        print("\n");
        show_toast(txt("Failed to save settings", "设置保存失败"));
    }
}

/// OOBE 完成时预写的一体化桌面配置（"dark,w:2,zh"）：launcher 首帧单次读取，
/// 避免逐个打开 theme/lang 文件；缺失时回退到逐文件读取。
const DESKTOP_META_PATH: &str = "/data/oobe/desktop_meta";

/// 读取 OOBE 与设置页保存的外观/快捷设置。
/// 桌面深浅色：全系统固定浅色（白色背景打底），不再读取主题字段。
fn load_preferences() {
    let mut buf = [0u8; 64];
    let n = unsafe { read_file(DESKTOP_META_PATH, &mut buf) };
    if n > 0 {
        let data = &buf[..n as usize];
        unsafe {
            THEME_DARK = false;
            for wallpaper in 0..4 {
                let marker = [b'w', b':', b'0' + wallpaper as u8];
                if bytes_contains(data, &marker) {
                    WALLPAPER = wallpaper;
                }
            }
            LANG_ZH = bytes_contains(data, b"zh");
        }
    } else {
        // 降级：desktop_meta 缺失（OOBE 未跑或旧版本），逐文件读取。
        let n = unsafe { read_file(THEME_PATH, &mut buf) };
        if n > 0 {
            let data = &buf[..n as usize];
            unsafe {
                THEME_DARK = false;
                for wallpaper in 0..4 {
                    let marker = [
                        b'w',
                        b'a',
                        b'l',
                        b'l',
                        b'p',
                        b'a',
                        b'p',
                        b'e',
                        b'r',
                        b':',
                        b'0' + wallpaper as u8,
                    ];
                    if bytes_contains(data, &marker) {
                        WALLPAPER = wallpaper;
                    }
                }
            }
        }
        let n = unsafe { read_file(LANG_PATH, &mut buf) };
        if n > 0 {
            unsafe {
                LANG_ZH = bytes_contains(&buf[..n as usize], b"zh");
            }
        }
    }
    unsafe {
        WIFI_ENABLED = read_number(WIFI_PATH, 1) != 0;
        // 默认 100（全亮度）：flip 走纯 memcpy 路径而非逐像素 LUT，
        // 整屏翻转从 ~150 万次查表降到一次 2MB 拷贝（TCG 下收益显著）。
        BRIGHTNESS = read_number(BRIGHTNESS_PATH, 100).clamp(25, 100);
        GRID_COLS = if read_number(GRID_PATH, 4) == 5 { 5 } else { 4 };
        let n = read_file(FOLDER_NAME_PATH, &mut FOLDER_NAME);
        if n > 0 {
            FOLDER_NAME_LEN = (n as usize).min(FOLDER_NAME.len());
        }
    }
    // 控制中心开关状态（飞行/WiFi/蓝牙/蜂窝 + 音量）。
    control_center::cc_load();
}

// ==================== 锁屏（安全配置加载与解锁逻辑） ====================

/// FNV-1a 32 位哈希 → 8 字节小写 hex（与 OOBE 保存的 security_hash 一致）。
fn fnv1a_hex(data: &[u8], out: &mut [u8; 8]) {
    let mut h: u32 = 0x811c9dc5;
    for &b in data {
        h ^= b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for i in 0..8 {
        let nib = (h >> ((7 - i) * 4)) & 0xF;
        out[i] = HEX[nib as usize];
    }
}

/// 读取 OOBE 安全配置并决定是否在启动时上锁。
fn load_security() {
    unsafe {
        let mut buf = [0u8; 16];
        let n = read_file(SECURITY_TYPE_PATH, &mut buf);
        if n > 0 {
            if bytes_contains(&buf[..n as usize], b"pattern") {
                LOCK_SEC_TYPE = 2;
            } else if bytes_contains(&buf[..n as usize], b"password") {
                LOCK_SEC_TYPE = 3;
            } else if bytes_contains(&buf[..n as usize], b"pin") {
                LOCK_SEC_TYPE = 1;
            }
        }
        let n = read_file(SECURITY_HASH_PATH, &mut buf);
        if n >= 8 {
            SEC_HASH.copy_from_slice(&buf[..8]);
            // 空密码（fnv1a("") = 811c9dc5）视为未设置安全：避免锁死后无法解锁。
            if SEC_HASH == *b"811c9dc5" {
                LOCK_SEC_TYPE = 0;
            }
        }
        let n = read_file(DURESS_HASH_PATH, &mut buf);
        if n >= 8 {
            DURESS_HASH.copy_from_slice(&buf[..8]);
        }
        let n = read_file(DURESS_ACTION_PATH, &mut buf);
        if n > 0 {
            if bytes_contains(&buf[..n as usize], b"wipe") {
                LOCK_DURESS_ACTION = 1;
            } else if bytes_contains(&buf[..n as usize], b"alert") {
                LOCK_DURESS_ACTION = 2;
            }
        }
        let n = read_file(LOCKED_PATH, &mut buf);
        let remote_locked = n > 0 && buf[0] == b'1';
        // 设置了安全方式（PIN/图案/密码）或远程锁定 → 启动即进入锁屏。
        LOCKED = LOCK_SEC_TYPE != 0 || remote_locked;
        if LOCKED {
            print("[lock] 开机即锁屏（秒=");
            print_dec(LOCK_SEC_TYPE as u32);
            print(")\n");
        }
    }
}

/// 手动锁屏（快捷面板「锁屏」按钮）。
fn lock_now() {
    unsafe {
        LOCKED = true;
        LOCK_INPUT_LEN = 0;
        LOCK_PATTERN_LEN = 0;
        LOCK_FAIL_TICKS = 0;
        SHADE_OPEN = false;
        // 锁定转场（ColorOS 风格）：锁屏整层从屏幕上方下滑入位，
        // 完成后由 lock_advance 收尾（转场期间输入已被屏蔽）。
        LOCK_FROM = -(DISPLAY_H as f32);
        LOCK_LAYER_OFF = -(DISPLAY_H as f32);
        LOCK_PHASE = LockPhase::Locking;
        LOCK_T0 = now_us();
        NEEDS_REDRAW = true;
        print("[lock] 屏幕已锁定\n");
    }
}

/// 锁屏转场/回弹推进。返回 true 表示动画进行中（需要继续重绘）。
fn lock_advance(now: u64) -> bool {
    unsafe {
        match LOCK_PHASE {
            LockPhase::None => false,
            LockPhase::Unlocking => {
                // 锁屏层上滑出屏（300ms FastOutSlowIn）。
                let t = (now.saturating_sub(LOCK_T0)) as f32 / 300_000.0;
                let e = Easing::FastOutSlowIn.sample(t.min(1.0));
                LOCK_LAYER_OFF = LOCK_FROM + (-(DISPLAY_H as f32) - LOCK_FROM) * e;
                if t >= 1.0 {
                    LOCKED = false;
                    LOCK_PHASE = LockPhase::None;
                    LOCK_LAYER_OFF = 0.0;
                    NEEDS_REDRAW = true;
                    return false;
                }
                true
            }
            LockPhase::Locking => {
                // 锁屏层从屏幕上方下滑入位（320ms）。
                let t = (now.saturating_sub(LOCK_T0)) as f32 / 320_000.0;
                let e = Easing::FastOutSlowIn.sample(t.min(1.0));
                LOCK_LAYER_OFF = LOCK_FROM + (0.0 - LOCK_FROM) * e;
                if t >= 1.0 {
                    LOCK_PHASE = LockPhase::None;
                    LOCK_LAYER_OFF = 0.0;
                    NEEDS_REDRAW = true;
                    return false;
                }
                true
            }
            LockPhase::Rebound => {
                // 上滑跟手未达阈值的回弹（250ms）。
                let t = (now.saturating_sub(LOCK_T0)) as f32 / 250_000.0;
                let e = Easing::EaseOut.sample(t.min(1.0));
                LOCK_LAYER_OFF = LOCK_FROM * (1.0 - e);
                if t >= 1.0 {
                    LOCK_PHASE = LockPhase::None;
                    LOCK_LAYER_OFF = 0.0;
                    NEEDS_REDRAW = true;
                    return false;
                }
                true
            }
        }
    }
}

/// 滑动解锁松手结算：位移越过 18% 屏高或上滑速度足够 → 解锁转场，
/// 否则从当前位置回弹到 0。
fn lock_swipe_settle(dy: i32, dur_ms: u64) {
    unsafe {
        if LOCK_PHASE != LockPhase::None {
            return;
        }
        let h = DISPLAY_H as f32;
        if !LOCK_DRAG {
            // 未进入跟手（快速轻扫直接抬起）：按手势位移判定。
            if dy < 0 {
                LOCK_FROM = 0.0;
                unlock_success();
            }
            return;
        }
        LOCK_DRAG = false;
        let off = LOCK_LAYER_OFF;
        let vel = if dur_ms > 0 {
            dy as f32 / dur_ms as f32 * 1000.0
        } else {
            0.0
        };
        if off < -h * 0.18 || vel < -600.0 {
            LOCK_FROM = off;
            unlock_success();
        } else {
            LOCK_FROM = off;
            LOCK_PHASE = LockPhase::Rebound;
            LOCK_T0 = now_us();
            NEEDS_REDRAW = true;
        }
    }
}

/// 当前锁屏已输入内容（PIN/密码取 LOCK_INPUT，图案取 1..9 序列）的哈希。
fn lock_input_hash(out: &mut [u8; 8]) {
    unsafe {
        let mut data = [0u8; 16];
        let mut n = 0usize;
        if LOCK_SEC_TYPE == 2 {
            for i in 0..LOCK_PATTERN_LEN {
                data[n] = b'0' + LOCK_PATTERN[i];
                n += 1;
            }
        } else {
            for i in 0..LOCK_INPUT_LEN {
                data[n] = LOCK_INPUT[i];
                n += 1;
            }
        }
        fnv1a_hex(&data[..n], out);
    }
}

fn lock_input_display_len() -> usize {
    unsafe {
        if LOCK_SEC_TYPE == 2 {
            LOCK_PATTERN_LEN
        } else {
            LOCK_INPUT_LEN
        }
    }
}

/// 解锁成功：清除锁定标志、清远程锁定、重绘桌面。
fn unlock_success() {
    unsafe {
        LOCK_INPUT_LEN = 0;
        LOCK_PATTERN_LEN = 0;
        LOCK_FAIL_TICKS = 0;
        write_file(LOCKED_PATH, b"0");
        print("[lock] 已解锁\n");
        // 解锁转场（ColorOS 风格）：锁屏整层从当前位置上滑出屏，露出桌面；
        // 完成后由 lock_advance 释放 LOCKED（转场期间输入已被屏蔽）。
        LOCK_FROM = LOCK_LAYER_OFF;
        LOCK_DRAG = false;
        LOCK_PHASE = LockPhase::Unlocking;
        LOCK_T0 = now_us();
        NEEDS_REDRAW = true;
    }
}

/// 解锁失败：进入错误提示（3 秒后清除），并清空已输入内容。
fn unlock_fail() {
    unsafe {
        LOCK_INPUT_LEN = 0;
        LOCK_PATTERN_LEN = 0;
        LOCK_FAIL_TICKS = 180; // 60fps × 3s
        print("[lock] 口令错误\n");
        NEEDS_REDRAW = true;
    }
}

/// 胁迫 PIN/图案被输入：按 duress_action 执行（fake_system 伪解锁 /
/// wipe 清除数据 / alert 提示）。
fn duress_triggered() {
    unsafe {
        match LOCK_DURESS_ACTION {
            1 => {
                // 清除数据：删除 OOBE 配置（重启后重新走 OOBE）。
                print("[lock] 胁迫：擦除数据\n");
                let _ = write_file(LOCKED_PATH, b"0");
                unlock_success();
                show_toast(txt("Data wiped", "数据已清除"));
            }
            2 => {
                print("[lock] 胁迫：已发送求助\n");
                unlock_fail();
                show_toast(txt("Alert sent to security", "警报已上报"));
            }
            _ => {
                // fake_system：假装解锁，但桌面保持锁定。
                print("[lock] 胁迫：伪装系统\n");
                LOCK_INPUT_LEN = 0;
                LOCK_PATTERN_LEN = 0;
                show_toast(txt("System is busy", "系统繁忙，请稍候"));
                NEEDS_REDRAW = true;
            }
        }
    }
}

/// 校验当前输入并解锁 / 触发胁迫动作。
///
/// `force=true`（确定键 / 图案松手 / 输入达上限）时不匹配立即失败；
/// 否则只做「哈希匹配即解锁」的增量校验，允许任意长度的 PIN/密码继续输入
/// —— 此前 PIN 输满 4 位即判失败，会把 OOBE 设置的 6 位及以上密码误判为
/// 错误（输到第 4 位就被清空重来，表现为「填进去提示密码错误」）。
fn lock_try_unlock(force: bool) {
    unsafe {
        if lock_input_display_len() == 0 {
            return;
        }
        let mut h = [0u8; 8];
        lock_input_hash(&mut h);
        // 胁迫 PIN（duress_hash）优先于正常密码。
        if DURESS_HASH != [0; 8] && h == DURESS_HASH {
            duress_triggered();
            return;
        }
        if h == SEC_HASH {
            unlock_success();
            return;
        }
        if force {
            unlock_fail();
        }
    }
}

fn page_count() -> usize {
    PAGE_COUNT
}

fn show_toast(text: &'static [u8]) {
    unsafe {
        TOAST = text;
        TOAST_T0 = now_us();
        TOAST_UNTIL = now_us() + 2_000_000;
        NEEDS_REDRAW = true;
    }
}

/// 判断逗号分隔串 `data` 是否包含 token `key`。
fn token_present(data: &[u8], key: &[u8]) -> bool {
    let mut pos = 0usize;
    while pos < data.len() {
        while pos < data.len() && is_sep(data[pos]) {
            pos += 1;
        }
        let start = pos;
        while pos < data.len() && data[pos] != b',' && !is_sep(data[pos]) {
            pos += 1;
        }
        let mut end = pos;
        while end > start && is_sep(data[end - 1]) {
            end -= 1;
        }
        if end - start == key.len() && data[start..end].iter().zip(key).all(|(a, b)| a == b) {
            return true;
        }
        while pos < data.len() && data[pos] != b',' {
            pos += 1;
        }
        if pos < data.len() {
            pos += 1;
        }
    }
    false
}

fn is_sep(c: u8) -> bool {
    c == b' ' || c == b'\t' || c == b'\r' || c == b'\n'
}

/// 读取 /data/oobe/modules 与 modules_removed，构建可见应用列表与桌面布局。
static mut MOD_BUF: [u8; 512] = [0u8; 512];
static mut REM_BUF: [u8; 512] = [0u8; 512];
static mut MOD_LEN: isize = -1;
static mut REM_LEN: isize = -1;

fn app_index_by_key(key: &str) -> Option<usize> {
    APPS.iter().position(|a| a.key == key)
}

fn app_visible(idx: usize) -> bool {
    let key = APPS[idx].key.as_bytes();
    unsafe {
        let mods_ok = if MOD_LEN > 0 {
            token_present(&MOD_BUF[..MOD_LEN as usize], key)
        } else {
            true
        };
        let not_removed = !(REM_LEN > 0 && token_present(&REM_BUF[..REM_LEN as usize], key));
        mods_ok && not_removed
    }
}

fn in_dock(idx: usize) -> bool {
    unsafe {
        for i in 0..DOCK_COUNT {
            if DOCK_APPS[i] == idx {
                return true;
            }
        }
    }
    false
}

/// 重建桌面页与系统工具文件夹内容（模块过滤后调用）。
fn rebuild_pages() {
    unsafe {
        // 第一屏：时钟小组件（顶部 4x2）+ 常用应用
        PAGES[0].count = 0;
        PAGES[0].items[0] = Item {
            kind: ItemKind::Widget(WidgetKind::Clock),
        };
        PAGES[0].count = 1;
        for &key in PAGE0_APPS.iter() {
            if let Some(idx) = app_index_by_key(key) {
                if app_visible(idx) && !in_dock(idx) && PAGES[0].count < MAX_PAGE_ITEMS {
                    PAGES[0].items[PAGES[0].count] = Item {
                        kind: ItemKind::App(idx),
                    };
                    PAGES[0].count += 1;
                }
            }
        }
        // 第二屏：系统工具文件夹 + 小组件 + 其他应用
        PAGES[1].count = 0;
        let seq: [ItemKind; 5] = [
            ItemKind::Folder,
            ItemKind::Widget(WidgetKind::Battery),
            ItemKind::Widget(WidgetKind::Weather),
            ItemKind::Widget(WidgetKind::Calendar),
            ItemKind::Widget(WidgetKind::Gallery),
        ];
        for k in seq {
            PAGES[1].items[PAGES[1].count] = Item { kind: k };
            PAGES[1].count += 1;
        }
        for &key in PAGE1_APPS.iter() {
            if let Some(idx) = app_index_by_key(key) {
                if app_visible(idx) && !in_dock(idx) && PAGES[1].count < MAX_PAGE_ITEMS {
                    PAGES[1].items[PAGES[1].count] = Item {
                        kind: ItemKind::App(idx),
                    };
                    PAGES[1].count += 1;
                }
            }
        }
        // 系统工具文件夹内容
        FOLDER_COUNT = 0;
        for &key in FOLDER_KEYS.iter() {
            if let Some(idx) = app_index_by_key(key) {
                if app_visible(idx) && FOLDER_COUNT < FOLDER_MAX {
                    FOLDER_APPS[FOLDER_COUNT] = idx;
                    FOLDER_COUNT += 1;
                }
            }
        }
    }
}

/// 冷路径诊断：启动时打印网格与 Dock 布局（应用 key + 点击中心），供宿主
/// 回归脚本按真实坐标点击 —— 模块过滤 / 网格列数变化都会让固定坐标失效。
fn dump_layout() {
    unsafe {
        for p in 0..PAGE_COUNT {
            let n = layout_page(&PAGES[p], GRID_COLS, &mut PLACED);
            for i in 0..n {
                if let ItemKind::App(idx) = PLACED[i].kind {
                    let (x, y, w, _h) = item_rect(&PLACED[i], GRID_COLS);
                    let app = &APPS[idx];
                    print("[launcher] tile ");
                    print(app.key);
                    print(" page=");
                    print_dec(p as u32);
                    print(" cx=");
                    print_dec((x + w / 2) as u32);
                    print(" cy=");
                    print_dec((y + w / 2) as u32);
                    print("\n");
                }
            }
        }
        layout_current();
        for i in 0..DOCK_COUNT {
            let (dx, dy) = dock_rect(i);
            let app = &APPS[DOCK_APPS[i]];
            print("[launcher] dock ");
            print(app.key);
            print(" cx=");
            print_dec((dx + DOCK_ICON / 2) as u32);
            print(" cy=");
            print_dec((dy + DOCK_ICON / 2) as u32);
            print("\n");
        }
    }
}

fn filter_modules() {
    unsafe {
        MOD_LEN = read_file(MODULES_PATH, &mut MOD_BUF);
        REM_LEN = read_file(REMOVED_PATH, &mut REM_BUF);
        VISIBLE_COUNT = 0;
        for i in 0..APPS.len() {
            if app_visible(i) {
                VISIBLE[VISIBLE_COUNT] = i;
                VISIBLE_COUNT += 1;
            }
        }
        // 固定 Dock：电话、短信、浏览器、相机、设置（尊重 modules_removed）。
        DOCK_COUNT = 0;
        for &key in DOCK_KEYS.iter() {
            if let Some(idx) = app_index_by_key(key) {
                let removed =
                    REM_LEN > 0 && token_present(&REM_BUF[..REM_LEN as usize], key.as_bytes());
                if !removed && DOCK_COUNT < DOCK_APPS.len() {
                    DOCK_APPS[DOCK_COUNT] = idx;
                    DOCK_COUNT += 1;
                }
            }
        }
        rebuild_pages();
        dump_layout();
        print("[launcher] 已加载 ");
        print_dec(VISIBLE_COUNT as u32);
        print(" 个模块\n");
    }
}

// ==================== 帧缓冲初始化 ====================

/// 打开 /dev/fb0、查询分辨率并 mmap 到用户空间；成功返回 true。
/// 失败分支分别打印原因（open 失败带返回码 / mmap 失败带返回值），便于区分
/// "全局打开文件表耗尽（open -EMFILE 等）"与 "ENOMEM（mmap）"。
fn fb_init() -> bool {
    unsafe {
        let fd = open(FB_PATH, O_RDWR);
        if fd < 0 {
            // 负返回码 = -errno：open 侧常见 -24（EMFILE，全局打开文件表耗尽）。
            print("[launcher] open /dev/fb0 失败，ret=-");
            print_dec(fd.unsigned_abs() as u32);
            print("\n");
            return false;
        }
        // 查询分辨率（24 字节：xres/yres/xres_virtual/yres_virtual/bpp/stride）
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
            // 合法性钳制：异常分辨率（0 / 超大）会让下方 FB_W*FB_H*4 在
            // release 下回绕成 0 → mmap 收到 len=0 恒 EINVAL（此前观测到的
            // 偶发 EINVAL 的可疑路径）。非法值一律保留默认 540x960。
            let sane = w > 0 && h > 0 && w <= 4096 && h <= 4096;
            if sane {
                FB_W = w;
                FB_H = h;
                FB_STRIDE_PX = if stride >= w * 4 { stride / 4 } else { w };
            } else {
                print("[launcher] fb0 ioctl 分辨率异常 w=");
                print_dec(w as u32);
                print(" h=");
                print_dec(h as u32);
                print("，保留默认\n");
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
            print("[launcher] mmap /dev/fb0 失败，ret=-");
            print_dec(((base as isize).unsigned_abs()) as u32);
            print("\n");
            return false;
        }
        FB = base as *mut u32;
        // 保留 fd：FBIO_PRESENT 提交脏区依赖它（映射建立后页表已指向画布，
        // 关闭 fd 不影响 mmap，但 ioctl 需要有效 fd）。
        FB_FD = fd as isize;
        true
    }
}

/// 触发内核侧启动画面淡出：桌面首帧渲染完成后调用一次；无启动画面时内核
/// 直接返回（open/ioctl 失败也静默跳过，不阻塞启动流程）。
fn splash_fade() {
    unsafe {
        let fd = open(FB_PATH, O_RDWR);
        if fd < 0 {
            return;
        }
        syscall3(SYS_IOCTL, fd as usize, FBIO_FADE_SPLASH, 0);
        syscall1(SYS_CLOSE, fd as usize);
    }
}

// ==================== 绘制原语 ====================

/// 图标瓦片渲染模式：为 true 时 put_pixel 写入 ICON_TILE（ICON_SIZE x ICON_SIZE），
/// 用于启动动画「图标方块放大覆盖屏幕」预渲染圆角图标与编辑模式抖动。
static mut TILE_MODE: bool = false;
static mut ICON_TILE: [u32; (ICON_SIZE as usize) * (ICON_SIZE as usize)] =
    [0u32; (ICON_SIZE as usize) * (ICON_SIZE as usize)];

// ==================== 图标缓存（渐进式启动预渲染） ====================
//
// 每个应用一个 ICON_SIZE x ICON_SIZE 瓦片缓存：启动时分批构建（每批 4 个），
// 此后网格/Dock 直接 blit 缓存而非逐像素重算圆角矩形，降低整屏重绘成本。

// 图标瓦片缓存按可见应用上限（VISIBLE [usize; 24]）扩容，避免模块列表超过
// 20 个可见应用时 ICON_CACHE_READY[idx] 越界（OOBE 完成后 21 个应用 → idx=20
// PANIC）。超过 24 时走 draw_icon_at 实时绘制回退。
static mut ICON_CACHE: [[u32; (ICON_SIZE as usize) * (ICON_SIZE as usize)]; 24] =
    [[0; (ICON_SIZE as usize) * (ICON_SIZE as usize)]; 24];
static mut ICON_CACHE_READY: [bool; 24] = [false; 24];

// ==================== 文件夹面板瓦片 / 小组件毛玻璃缓冲 ====================

static mut FOLDER_TILE: [u32; FOLDER_PANEL_W * FOLDER_PANEL_H] =
    [0; FOLDER_PANEL_W * FOLDER_PANEL_H];
static mut FOLDER_ORIG: [u32; FOLDER_PANEL_W * FOLDER_PANEL_H] =
    [0; FOLDER_PANEL_W * FOLDER_PANEL_H];
/// 文件夹瓦片渲染模式：为 true 时 put_pixel 写入 FOLDER_TILE（面板原点到屏幕映射）。
static mut FOLDER_TILE_ACTIVE: bool = false;
static mut FOLDER_TILE_X: i32 = 0;
static mut FOLDER_TILE_Y: i32 = 0;

const WIDGET_SCRATCH_W: usize = 320;
const WIDGET_SCRATCH_H: usize = 160;
static mut WIDGET_SCRATCH: [u32; WIDGET_SCRATCH_W * WIDGET_SCRATCH_H] =
    [0; WIDGET_SCRATCH_W * WIDGET_SCRATCH_H];

/// 文件夹打开时全屏压暗 LUT（blend(c, 0x00050A14, 150)）。
static mut DIM_LUT: [u32; 256] = [0; 256];
static mut DIM_LUT_READY: bool = false;

/// 星期「周X」组合缓冲。
static mut WEEK_BUF: [u8; 6] = [0; 6];
static mut WEEK_BUF_LEN: usize = 0;

/// 桌面应用图标贴图文件缓冲（/share/app_icons.rgba，启动时读入后零拷贝
/// 借给 app_icons 模块；不放 ELF 内嵌以免超过内核 execve 缓冲上限）。
static mut ICON_FILE_BUF: [u8; app_icons::ALL_BYTES] = [0; app_icons::ALL_BYTES];

// ==================== 渐进式启动状态 ====================
//
// BOOT_ACTIVE 期间 render_frame 按 BOOT_LIMIT 增量绘制图标并叠加加载指示器；
// 完成全部批次后 BOOT_ACTIVE=false，进入常规渲染路径。

static mut BOOT_ACTIVE: bool = false;
/// 已绘制的网格槽位数（0..=BOOT_TOTAL）。
static mut BOOT_LIMIT: usize = 0;
/// 本次启动需加载的槽位总数（<= 单页 12 个）。
static mut BOOT_TOTAL: usize = 0;
/// 已构建缓存瓦片的可见应用数。
static mut BOOT_BUILT: usize = 0;

/// 全局像素写入透明度（255 = 不透明）。Toast 等浮层淡入淡出期间临时降低，
/// put_pixel 写 BACK_BUF 时按它做 alpha 混合，绘制完毕即恢复 255。
/// 瓦片渲染路径（图标/文件夹预渲染）不受影响（浮层不会与瓦片模式并存）。
static mut PIX_ALPHA: u32 = 255;

fn put_pixel(x: i32, y: i32, color: u32) {
    // 最高位字节可携带逐像素 alpha（烘焙图标贴图的半透明边缘），0 保持既有
    // 约定的不透明色。TILE_MODE 原样写入（保留 alpha 供旋转/缩放二次合成），
    // 屏幕与文件夹瓦片按目标现值混合。
    let a = (color >> 24) & 0xFF;
    unsafe {
        if FOLDER_TILE_ACTIVE {
            let lx = x - FOLDER_TILE_X;
            let ly = y - FOLDER_TILE_Y;
            if lx >= 0 && ly >= 0 && lx < FOLDER_PANEL_W as i32 && ly < FOLDER_PANEL_H as i32 {
                let di = ly as usize * FOLDER_PANEL_W + lx as usize;
                FOLDER_TILE[di] = if a > 0 {
                    blend(FOLDER_TILE[di], color & 0x00_FF_FF_FF, a)
                } else {
                    color
                };
            }
            return;
        }
        if TILE_MODE {
            if x >= 0 && y >= 0 && x < ICON_SIZE && y < ICON_SIZE {
                ICON_TILE[y as usize * ICON_SIZE as usize + x as usize] = color;
            }
            return;
        }
        if x < 0 || y < 0 || x >= DISPLAY_W as i32 || y >= DISPLAY_H as i32 {
            return;
        }
        let idx = y as usize * DISPLAY_W + x as usize;
        BACK_BUF[idx] = if a > 0 {
            let base = blend(BACK_BUF[idx], color & 0x00_FF_FF_FF, a);
            if PIX_ALPHA >= 255 {
                base
            } else {
                blend(BACK_BUF[idx], base, PIX_ALPHA)
            }
        } else if PIX_ALPHA >= 255 {
            color
        } else {
            blend(BACK_BUF[idx], color, PIX_ALPHA)
        };
    }
}

/// 读取当前渲染目标上的像素（屏幕 BACK_BUF 或文件夹瓦片）。
fn current_pixel(x: i32, y: i32) -> u32 {
    unsafe {
        if FOLDER_TILE_ACTIVE {
            let lx = x - FOLDER_TILE_X;
            let ly = y - FOLDER_TILE_Y;
            if lx >= 0 && ly >= 0 && lx < FOLDER_PANEL_W as i32 && ly < FOLDER_PANEL_H as i32 {
                return FOLDER_TILE[ly as usize * FOLDER_PANEL_W + lx as usize];
            }
            return 0;
        }
        if x >= 0 && y >= 0 && x < DISPLAY_W as i32 && y < DISPLAY_H as i32 {
            BACK_BUF[y as usize * DISPLAY_W + x as usize]
        } else {
            0
        }
    }
}

/// 把 back buffer 一次性拷贝到显存。
///
/// 重建亮度 LUT（亮度级别变化时调用；首次 flip 也会自动触发）。
fn rebuild_brightness_lut() {
    unsafe {
        let level = BRIGHTNESS;
        for v in 0..256usize {
            BRIGHTNESS_LUT[v] = v as u32 * level / 100;
        }
        BRIGHTNESS_LUT_LEVEL = level;
    }
}

/// 把 back buffer 的变更矩形拷贝到显存并提交呈现（脏区域直绘路径）。
///
/// 未显式调用 `mark_dirty` 时退化为整屏拷贝 + 整屏脏区（与旧行为一致）；
/// 调用过则只拷贝/提交已标记的矩形 —— 时钟/光标/通知等局部更新从 2MB
/// 整屏 memcpy 降到矩形级拷贝。
/// 亮度 < 100 时经 LUT 查表缩放（亮度改变时重建，避免每帧每像素乘除）。
fn flip() {
    unsafe {
        if FB.is_null() {
            return;
        }
        if BRIGHTNESS_LUT_LEVEL != BRIGHTNESS {
            rebuild_brightness_lut();
        }
        if DIRTY_COUNT == 0 && !DIRTY_FULL {
            DIRTY_FULL = true;
        }
        if DIRTY_FULL {
            if FB_W == DISPLAY_W && FB_H == DISPLAY_H && FB_STRIDE_PX == DISPLAY_W {
                if BRIGHTNESS >= 100 {
                    core::ptr::copy_nonoverlapping(BACK_BUF.as_ptr(), FB, DISPLAY_W * DISPLAY_H);
                } else {
                    for i in 0..DISPLAY_W * DISPLAY_H {
                        let p = BACK_BUF[i];
                        let r = BRIGHTNESS_LUT[((p >> 16) & 0xFF) as usize];
                        let g = BRIGHTNESS_LUT[((p >> 8) & 0xFF) as usize];
                        let b = BRIGHTNESS_LUT[(p & 0xFF) as usize];
                        *FB.add(i) = (r << 16) | (g << 8) | b;
                    }
                }
            } else {
                let w = FB_W.min(DISPLAY_W);
                let h = FB_H.min(DISPLAY_H);
                for y in 0..h {
                    for x in 0..w {
                        let p = BACK_BUF[y * DISPLAY_W + x];
                        let r = BRIGHTNESS_LUT[((p >> 16) & 0xFF) as usize];
                        let g = BRIGHTNESS_LUT[((p >> 8) & 0xFF) as usize];
                        let b = BRIGHTNESS_LUT[(p & 0xFF) as usize];
                        *FB.add(y * FB_STRIDE_PX + x) = (r << 16) | (g << 8) | b;
                    }
                }
            }
        } else {
            // 只拷贝已标记的变更矩形（行内整行拷贝，越界裁剪）
            for i in 0..DIRTY_COUNT {
                let d = DIRTY[i];
                let x0 = d.0.max(0);
                let y0 = d.1.max(0);
                // ★ 两边都要钳制：只钳 FB_W/FB_H 不够 —— 内核帧缓冲可能大于
                // BACK_BUF（DISPLAY_W×DISPLAY_H），此时矩形越出 BACK_BUF 行尾/
                // 末行，copy_nonoverlapping 静默越界读（同 app_kit flip 的修法）
                let x1 = (d.0 + d.2 as i32).min(FB_W as i32).min(DISPLAY_W as i32);
                let y1 = (d.1 + d.3 as i32).min(FB_H as i32).min(DISPLAY_H as i32);
                if x1 <= x0 || y1 <= y0 {
                    continue;
                }
                let rw = (x1 - x0) as usize;
                if BRIGHTNESS >= 100 {
                    for y in y0..y1 {
                        core::ptr::copy_nonoverlapping(
                            BACK_BUF.as_ptr().add(y as usize * DISPLAY_W + x0 as usize),
                            FB.add(y as usize * FB_STRIDE_PX + x0 as usize),
                            rw,
                        );
                    }
                } else {
                    for y in y0..y1 {
                        for x in 0..rw {
                            let p = BACK_BUF[y as usize * DISPLAY_W + (x0 as usize + x)];
                            let r = BRIGHTNESS_LUT[((p >> 16) & 0xFF) as usize];
                            let g = BRIGHTNESS_LUT[((p >> 8) & 0xFF) as usize];
                            let b = BRIGHTNESS_LUT[(p & 0xFF) as usize];
                            *FB.add(y as usize * FB_STRIDE_PX + x0 as usize + x) =
                                (r << 16) | (g << 8) | b;
                        }
                    }
                }
            }
        }
        fb_submit_present();
    }
}

fn fill_rect(x: i32, y: i32, w: i32, h: i32, color: u32) {
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

fn draw_rect(x: i32, y: i32, w: i32, h: i32, color: u32) {
    for dx in 0..w {
        put_pixel(x + dx, y, color);
        put_pixel(x + dx, y + h - 1, color);
    }
    for dy in 0..h {
        put_pixel(x, y + dy, color);
        put_pixel(x + w - 1, y + dy, color);
    }
}

/// 壁纸调色板（顶部渐变色、底部渐变色、棋盘强调色），按 (深色, 壁纸序号)。
fn wallpaper_palette(dark: bool, idx: usize) -> (u32, u32, u32) {
    match (dark, idx) {
        (false, 0) => (0x00_EF_F6_FF, 0x00_BF_DB_FE, 0x00_60_A5_FA),
        (false, 1) => (0x00_F0_FD_FA, 0x00_A7_F3_D0, 0x00_14_B8_A6),
        (false, 2) => (0x00_FF_F7_ED, 0x00_FE_D7_AA, 0x00_F9_73_16),
        (false, _) => (0x00_FA_F5_FF, 0x00_E9_D5_FF, 0x00_A8_55_F7),
        (true, 0) => (0x00_0B_10_20, 0x00_1E_3A_5F, 0x00_25_63_EB),
        (true, 1) => (0x00_0B_1F_1A, 0x00_13_46_3B, 0x00_0F_76_6E),
        (true, 2) => (0x00_1A_1A_2E, 0x00_16_21_3E, 0x00_EA_58_0C),
        (true, _) => (0x00_18_10_2A, 0x00_31_1B_53, 0x00_7E_22_CE),
    }
}

/// 壁纸切换交叉淡化（ColorOS 换壁纸过渡）：进行中时 draw_background 把
/// 旧调色板按缓动进度向新调色板混合（行级混合，逐像素零额外开销）。
static mut WP_FADE_ACTIVE: bool = false;
static mut WP_FADE_T0: u64 = 0;
static mut WP_FADE_FROM: (u32, u32, u32) = (0, 0, 0);

/// 壁纸淡化动画是否进行中（主循环唤醒条件）。
fn wallpaper_fade_active() -> bool {
    unsafe { WP_FADE_ACTIVE }
}

/// 应用返回前台时同步偏好；壁纸/主题调色板变化则启动 300ms 交叉淡化
/// （设置应用改动外观后回桌面即时生效，无需重启 launcher）。
fn wallpaper_sync_from_prefs() {
    unsafe {
        let old = wallpaper_palette(THEME_DARK, WALLPAPER);
        load_preferences();
        let new = wallpaper_palette(THEME_DARK, WALLPAPER);
        if old != new {
            WP_FADE_FROM = old;
            WP_FADE_T0 = now_us();
            WP_FADE_ACTIVE = true;
            NEEDS_REDRAW = true;
        }
    }
}

/// 当前壁纸调色板（含切换淡入淡出的行级混合），供整屏/局部背景绘制共用。
fn wallpaper_colors() -> (u32, u32, u32) {
    unsafe {
        let (t, b, a) = wallpaper_palette(THEME_DARK, WALLPAPER);
        if !WP_FADE_ACTIVE {
            (t, b, a)
        } else {
            let p = motion::progress(WP_FADE_T0, 300_000, now_us());
            if p >= 1.0 {
                WP_FADE_ACTIVE = false;
                (t, b, a)
            } else {
                // EaseOut：前段快后段缓的调色板过渡（行级混合，零逐像素开销）。
                let e = motion::Easing::EaseOut.eval(p);
                let k = (e * 255.0) as u32;
                let (ft, fb, fa) = WP_FADE_FROM;
                (blend(ft, t, k), blend(fb, b, k), blend(fa, a, k))
            }
        }
    }
}

/// 重绘矩形区域的壁纸背景（时钟小组件每秒局部刷新用，避免整屏 2MB 重绘）。
fn draw_background_rect(x0: i32, y0: i32, rw: u32, rh: u32) {
    let key = wallpaper_colors();
    let xa = x0.max(0);
    let ya = y0.max(0);
    let xb = (x0 + rw as i32).min(DISPLAY_W as i32);
    let yb = (y0 + rh as i32).min(DISPLAY_H as i32);
    unsafe {
        // 缓存命中：按行整行 memcpy（与整屏渲染逐位一致），免去逐像素渐变
        // +波形混合（时钟小组件每秒局部刷新从此走纯拷贝）。
        if BG_KEY == key {
            for y in ya..yb {
                let src = y as usize * DISPLAY_W + xa as usize;
                core::ptr::copy_nonoverlapping(
                    BG_BUF.as_ptr().add(src),
                    BACK_BUF.as_mut_ptr().add(src),
                    (xb - xa) as usize,
                );
            }
            return;
        }
        let (top, bottom, accent) = key;
        for y in ya..yb {
            let t = y as u32;
            let inv = DISPLAY_H as u32 - t;
            let r = (((top >> 16) & 0xFF) * inv + ((bottom >> 16) & 0xFF) * t) / DISPLAY_H as u32;
            let g = (((top >> 8) & 0xFF) * inv + ((bottom >> 8) & 0xFF) * t) / DISPLAY_H as u32;
            let b = ((top & 0xFF) * inv + (bottom & 0xFF) * t) / DISPLAY_H as u32;
            let row = (r << 16) | (g << 8) | b;
            for x in xa..xb {
                let wave = if ((x as u32 / 72 + t / 96) & 1) == 0 {
                    accent
                } else {
                    row
                };
                let color = if x as u32 % 72 < 2 && t > 180 {
                    blend(row, wave, 26)
                } else {
                    row
                };
                put_pixel(x, y, color);
            }
        }
    }
}

/// 渲染后的壁纸背景缓存（52 万像素的渐变+波形逐像素计算结果）。
///
/// 文件夹/翻页/控制中心/最近任务等动画每帧都经 `draw_desktop_full` 重画整屏
/// 背景，逐像素数学是动画帧率的最大单项开销（TCG 下 50~150ms/帧）。
/// 调色板键不变时（绝大多数动画帧）直接整块 memcpy，调色板变化（换壁纸
/// 交叉淡化期间每帧键都不同）才重新渲染并更新缓存。
static mut BG_BUF: [u32; DISPLAY_W * DISPLAY_H] = [0u32; DISPLAY_W * DISPLAY_H];
/// 缓存对应的调色板键（初始值不可由 blend 产出，强制首帧全量渲染）。
static mut BG_KEY: (u32, u32, u32) = (u32::MAX, u32::MAX, u32::MAX);

fn draw_background() {
    let key = wallpaper_colors();
    unsafe {
        let n = DISPLAY_W * DISPLAY_H;
        if BG_KEY == key {
            BACK_BUF[..n].copy_from_slice(&BG_BUF);
            return;
        }
        let (top, bottom, accent) = key;
        for y in 0..DISPLAY_H {
            let t = y as u32;
            let inv = DISPLAY_H as u32 - t;
            let r = (((top >> 16) & 0xFF) * inv + ((bottom >> 16) & 0xFF) * t) / DISPLAY_H as u32;
            let g = (((top >> 8) & 0xFF) * inv + ((bottom >> 8) & 0xFF) * t) / DISPLAY_H as u32;
            let b = ((top & 0xFF) * inv + (bottom & 0xFF) * t) / DISPLAY_H as u32;
            let row = (r << 16) | (g << 8) | b;
            let base = y * DISPLAY_W;
            for x in 0..DISPLAY_W {
                let wave = if ((x / 72 + y / 96) & 1) == 0 {
                    accent
                } else {
                    row
                };
                let color = if x % 72 < 2 && y > 180 {
                    blend(row, wave, 26)
                } else {
                    row
                };
                // 直接写缓存（等价 put_pixel 的不透明路径：颜色无逐像素
                // alpha、PIX_ALPHA=255；本函数只在桌面渲染路径调用，瓦片
                // 模式不会与之并发）。
                BG_BUF[base + x] = color;
            }
        }
        BG_KEY = key;
        BACK_BUF[..n].copy_from_slice(&BG_BUF);
    }
}

fn blend(a: u32, b: u32, alpha: u32) -> u32 {
    let alpha = alpha.min(255);
    let inv = 255 - alpha;
    let r = (((a >> 16) & 0xFF) * inv + ((b >> 16) & 0xFF) * alpha) / 255;
    let g = (((a >> 8) & 0xFF) * inv + ((b >> 8) & 0xFF) * alpha) / 255;
    let bl = ((a & 0xFF) * inv + (b & 0xFF) * alpha) / 255;
    (r << 16) | (g << 8) | bl
}

fn avg3(a: u32, b: u32, c: u32) -> u32 {
    let r = (((a >> 16) & 0xFF) + ((b >> 16) & 0xFF) + ((c >> 16) & 0xFF)) / 3;
    let g = (((a >> 8) & 0xFF) + ((b >> 8) & 0xFF) + ((c >> 8) & 0xFF)) / 3;
    let bl = ((a & 0xFF) + (b & 0xFF) + (c & 0xFF)) / 3;
    (r << 16) | (g << 8) | bl
}

// ==================== TTF 平滑字体（cn.ttf） ====================

/// 内置点阵回退的缩放系数（8x8 ASCII / 16x16 CJK → 目标字号 px）。
fn bitmap_scale(px: i32, native: i32) -> i32 {
    let s = (px + native - 1) / native;
    if s < 1 {
        1
    } else {
        s
    }
}

/// 内置点阵回退的推进宽度（与绘制一致）。
fn bitmap_advance(cp: u32, px: i32) -> i32 {
    if cp < 0x80 {
        8 * bitmap_scale(px, 8)
    } else if cp <= 0xFFFF && cjk_font::cjk_index(cp as u16).is_some() {
        16 * bitmap_scale(px, 16)
    } else {
        if px < 16 {
            8
        } else {
            16
        }
    }
}

/// 以内置点阵字库绘制单个码点（8x8 ASCII / 16x16 CJK，按字号缩放），
/// 覆盖到当前渲染目标；点阵中也没有的码点只推进宽度、不绘制任何方块。
/// 在 QEMU TCG 下 3MB cn.ttf 的 fontdue 解析会长时间占满 CPU（同步阻塞），
/// 与 OOBE 相同的策略：桌面文本管线在 font_ready()==false 时直接走点阵，
/// 字体可后续再加载。
fn draw_bitmap_scaled(x: i32, y: i32, cp: u32, px: i32, color: u32) -> i32 {
    if cp < 0x80 {
        let g = glyph(cp as u8);
        let s = bitmap_scale(px, 8);
        for row in 0..8 {
            for col in 0..8 {
                if g[row] & (0x80u8 >> col) != 0 {
                    for sy in 0..s {
                        for sx in 0..s {
                            put_pixel(
                                x + col as i32 * s + sx as i32,
                                y + row as i32 * s + sy as i32,
                                color,
                            );
                        }
                    }
                }
            }
        }
        return 8 * s;
    }
    let idx = if cp <= 0xFFFF { cjk_font::cjk_index(cp as u16) } else { None };
    if let Some(idx) = idx {
        let base = idx * 32;
        let s = bitmap_scale(px, 16);
        for row in 0..16 {
            let b0 = cjk_font::CJK_BITMAPS[base + row * 2];
            let b1 = cjk_font::CJK_BITMAPS[base + row * 2 + 1];
            for col in 0..16 {
                let bit = if col < 8 {
                    b0 & (0x80u8 >> col)
                } else {
                    b1 & (0x80u8 >> (col - 8))
                };
                if bit != 0 {
                    for sy in 0..s {
                        for sx in 0..s {
                            put_pixel(
                                x + col as i32 * s + sx as i32,
                                y + row as i32 * s + sy as i32,
                                color,
                            );
                        }
                    }
                }
            }
        }
        return 16 * s;
    }
    if cp >= 0x80 {
        16
    } else {
        8
    }
}

/// 把单个码点以 cn.ttf 平滑字形绘制到 (x, y)（y 为字形顶部），返回推进宽度。
fn ttf_glyph(x: i32, y: i32, cp: u32, color: u32) -> i32 {
    if !ttf_text::font_render::font_ready() {
        return draw_bitmap_scaled(x, y, cp, ttf_text::font_render::FONT_PX as i32, color);
    }
    let px = ttf_text::font_render::FONT_PX;
    let ch = core::char::from_u32(cp).unwrap_or(' ');
    if let Some(g) = ttf_text::font_render::rasterize_char(ch, px) {
        let gx = x + g.xmin;
        let gy = ttf_text::font_render::glyph_draw_top(y, g.ymin, g.height, px);
        for row in 0..g.height {
            for col in 0..g.width {
                let a = g.data[row * g.width + col];
                if a != 0 {
                    let px_ = gx + col as i32;
                    let py_ = gy + row as i32;
                    // 背景必须取自当前 back buffer（BACK_BUF）而不是显存 FB：
                    // FB 是上一帧 flip 的结果，用它做 alpha 混合会把上一帧的内容
                    // 混进本帧文字，产生「鬼画符」与错位。
                    let in_b =
                        px_ >= 0 && py_ >= 0 && px_ < DISPLAY_W as i32 && py_ < DISPLAY_H as i32;
                    if in_b {
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

fn draw_char(x: i32, y: i32, c: u8, color: u32) {
    ttf_glyph(x, y, c as u32, color);
}

/// 绘制单个 16x16 中文字形（`idx` 为 `cjk_font::cjk_index` 返回的索引）。
fn draw_cjk(x: i32, y: i32, cp: u16, color: u32) {
    ttf_glyph(x, y, cp as u32, color);
}

fn draw_text(x: i32, y: i32, s: &[u8], color: u32) {
    let mut cx = x;
    let mut i = 0;
    while i < s.len() {
        let (cp, n) = ttf_text::font_render::utf8_next(s, i);
        cx += ttf_glyph(cx, y, cp, color);
        i += n;
    }
}

fn text_width(s: &[u8]) -> i32 {
    if !ttf_text::font_render::font_ready() {
        return bitmap_text_width(s, ttf_text::font_render::FONT_PX as i32);
    }
    ttf_text::font_render::text_width(s, ttf_text::font_render::FONT_PX)
}

/// 内置点阵回退下的整行文本宽度（与 draw_bitmap_scaled 的推进一致）。
fn bitmap_text_width(s: &[u8], px: i32) -> i32 {
    let mut w = 0i32;
    let mut i = 0;
    while i < s.len() {
        let (cp, n) = ttf_text::font_render::utf8_next(s, i);
        w += bitmap_advance(cp, px);
        i += n;
    }
    w
}

/// 指定字号绘制单个码点（背景取当前渲染目标，支持文件夹瓦片内绘制）。
fn ttf_glyph_px(x: i32, y: i32, cp: u32, px: f32, color: u32) -> i32 {
    if !ttf_text::font_render::font_ready() {
        return draw_bitmap_scaled(x, y, cp, px as i32, color);
    }
    if let Some(ch) = core::char::from_u32(cp) {
        if let Some(g) = ttf_text::font_render::rasterize_char(ch, px) {
            let gx = x + g.xmin;
            let gy = ttf_text::font_render::glyph_draw_top(y, g.ymin, g.height, px);
            for row in 0..g.height {
                for col in 0..g.width {
                    let a = g.data[row * g.width + col];
                    if a != 0 {
                        let px_ = gx + col as i32;
                        let py_ = gy + row as i32;
                        if px_ >= 0 && py_ >= 0 && px_ < DISPLAY_W as i32 && py_ < DISPLAY_H as i32
                        {
                            let bg = current_pixel(px_, py_);
                            put_pixel(px_, py_, ttf_text::font_render::blend(bg, color, a));
                        }
                    }
                }
            }
            return g.advance;
        }
    }
    0
}

fn draw_text_px(x: i32, y: i32, s: &[u8], px: f32, color: u32) {
    let mut cx = x;
    let mut i = 0;
    while i < s.len() {
        let (cp, n) = ttf_text::font_render::utf8_next(s, i);
        cx += ttf_glyph_px(cx, y, cp, px, color);
        i += n;
    }
}

fn text_width_px(s: &[u8], px: f32) -> i32 {
    ttf_text::font_render::text_width(s, px)
}

/// 单行标签：超出 max_w 截断并追加省略号。
fn draw_label_trunc(x: i32, y: i32, s: &[u8], px: f32, max_w: i32, color: u32) {
    let mut w = 0i32;
    let mut i = 0;
    let mut truncated = false;
    while i < s.len() {
        let (cp, n) = ttf_text::font_render::utf8_next(s, i);
        let a = ttf_text::font_render::advance(cp, px).max(1);
        if w + a > max_w {
            truncated = true;
            break;
        }
        w += ttf_glyph_px(x + w, y, cp, px, color);
        i += n;
    }
    if truncated {
        let mut ex = x + w;
        for _ in 0..3 {
            ex += ttf_glyph_px(ex, y, b'.' as u32, px, color);
        }
    }
}

/// 按当前语言选择文本（默认中文）。
fn txt(en: &'static str, zh: &'static str) -> &'static [u8] {
    if unsafe { LANG_ZH } {
        zh.as_bytes()
    } else {
        en.as_bytes()
    }
}

fn isqrt(n: i32) -> i32 {
    if n <= 0 {
        return 0;
    }
    let mut x = n;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

fn draw_wifi(x: i32, y: i32, color: u32) {
    let cx = x + 7;
    let cy = y + 11;
    // 底部圆点
    put_pixel(cx, cy, color);
    put_pixel(cx + 1, cy, color);
    put_pixel(cx, cy + 1, color);
    put_pixel(cx + 1, cy + 1, color);
    // 三个同心圆弧（上半个圆）
    let mut r = 3;
    while r <= 7 {
        let mut dx = -r;
        while dx <= r {
            let dy = isqrt(r * r - dx * dx);
            put_pixel(cx + dx, cy - dy, color);
            dx += 1;
        }
        r += 2;
    }
}

fn draw_battery(x: i32, y: i32, color: u32) {
    // 主体 18x10，正极凸起 2x4，约 70% 电量
    draw_rect(x, y, 18, 10, color);
    fill_rect(x + 18, y + 3, 2, 4, color);
    fill_rect(x + 2, y + 2, 12, 6, color);
}

fn draw_icon_at(x: i32, y: i32, size: i32, color: u32) {
    let r = icon_radius(size).max(1);
    // 圆角背景 + 圆角描边（ColorOS 15 连续曲率超椭圆，30% 圆角）
    fill_round_rect(x, y, size, size, r, color);
    draw_round_rect(x, y, size, size, r, COLOR_ICON_BORDER);
}

/// 圆角比例 0.30：56px 图标 → 17px（ColorOS 15 超椭圆圆角矩形）。
fn icon_radius(size: i32) -> i32 {
    (size * 30) / 100
}

/// 判断相对圆角矩形左上角的点 (dx, dy) 是否在圆角矩形内
/// （超椭圆连续曲率几何，与系统 glass 模块一致）。
fn in_round_rect(dx: i32, dy: i32, w: i32, h: i32, r: i32) -> bool {
    glass::in_round_rect(dx, dy, w, h, r)
}

/// BACK_BUF 抗锯齿绘制目标（launcher 自有帧缓冲）。
struct LauncherSmooth;

impl glass::SmoothTarget for LauncherSmooth {
    fn put_aa(&mut self, x: i32, y: i32, color: u32, alpha: u32) {
        if x < 0 || y < 0 || x >= DISPLAY_W as i32 || y >= DISPLAY_H as i32 {
            return;
        }
        unsafe {
            let idx = y as usize * DISPLAY_W + x as usize;
            BACK_BUF[idx] = blend(BACK_BUF[idx], color, alpha);
        }
    }
}

/// 填充圆角矩形（抗锯齿超椭圆连续曲率，ColorOS 15 观感）。
fn fill_round_rect(x: i32, y: i32, w: i32, h: i32, r: i32, color: u32) {
    glass::smooth_fill_rr(&mut LauncherSmooth, x, y, w, h, r, color);
}

/// 绘制圆角矩形 1px 描边（抗锯齿）。
fn draw_round_rect(x: i32, y: i32, w: i32, h: i32, r: i32, color: u32) {
    glass::smooth_stroke_rr(&mut LauncherSmooth, x, y, w, h, r, color, 255);
}

fn draw_line(mut x0: i32, mut y0: i32, x1: i32, y1: i32, color: u32) {
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

/// 空心圆（抗锯齿 1px 圆环，锁屏图案点用）。
fn draw_circle(cx: i32, cy: i32, r: i32, color: u32) {
    glass::smooth_stroke_circle(&mut LauncherSmooth, cx, cy, r, color);
}

/// 以简单几何符号替代纯色块，缩放时仍保持可辨识。
/// 应用在 APPS 表中的下标（调用方传入的 &App 均来自 &APPS[i]）。
fn app_index_of(app: &App) -> usize {
    for i in 0..APPS.len() {
        if core::ptr::eq(app, &APPS[i]) {
            return i;
        }
    }
    usize::MAX
}

/// 烘焙贴图最近邻缩放绘制（Dock 52px / 按压反馈 / 占位页 96px 等非
/// ICON_SIZE 尺寸路径，以及瓦片缓存未预热时的 56px 直绘）。半透明边缘
/// 像素交由 put_pixel 与目标混合；无贴图数据返回 false（走几何回退）。
fn draw_icon_asset(app: &App, x: i32, y: i32, size: i32) -> bool {
    let idx = app_index_of(app);
    let src = match app_icons::icon_rgba(idx) {
        Some(s) => s,
        None => return false,
    };
    let n = ICON_SIZE as usize;
    let sz = size.max(1) as usize;
    for dy in 0..sz {
        let sy = dy * n / sz;
        for dx in 0..sz {
            let sx = dx * n / sz;
            let o = (sy * n + sx) * 4;
            let a = src[o + 3] as u32;
            if a == 0 {
                continue;
            }
            let rgb = ((src[o] as u32) << 16) | ((src[o + 1] as u32) << 8) | (src[o + 2] as u32);
            put_pixel(
                x + dx as i32,
                y + dy as i32,
                if a >= 255 { rgb } else { (a << 24) | rgb },
            );
        }
    }
    true
}

/// 预渲染第 idx 个应用的 ICON_SIZE x ICON_SIZE 图标瓦片到缓存：优先烘焙
/// 贴图（make_icons.py），无数据时回退 TILE_MODE 程序化绘制。
fn build_icon_cache_entry(idx: usize) {
    unsafe {
        if idx >= APPS.len() || ICON_CACHE_READY[idx] {
            return;
        }
        if !fill_icon_cache_from_asset(idx) {
            TILE_MODE = true;
            for px in ICON_TILE.iter_mut() {
                *px = 0;
            }
            draw_app_icon(&APPS[idx], 0, 0, ICON_SIZE);
            TILE_MODE = false;
            ICON_CACHE[idx].copy_from_slice(&ICON_TILE);
        }
        ICON_CACHE_READY[idx] = true;
    }
}

/// 烘焙贴图 → 图标瓦片缓存：不透明像素 0x00RRGGBB，半透明像素 alpha 编码
/// 进最高位字节（blit 时与背景混合），全透明 0（blit 跳过）。无该应用的
/// 贴图数据返回 false。
fn fill_icon_cache_from_asset(idx: usize) -> bool {
    let src = match app_icons::icon_rgba(idx) {
        Some(s) => s,
        None => return false,
    };
    unsafe {
        for px in 0..(ICON_SIZE as usize * ICON_SIZE as usize) {
            let o = px * 4;
            let a = src[o + 3] as u32;
            ICON_CACHE[idx][px] = if a == 0 {
                0
            } else {
                let rgb =
                    ((src[o] as u32) << 16) | ((src[o + 1] as u32) << 8) | (src[o + 2] as u32);
                if a >= 255 {
                    rgb
                } else {
                    (a << 24) | rgb
                }
            };
        }
    }
    true
}

/// 把缓存瓦片 blit 到 (x, y)（跳过透明角像素）。
fn blit_icon(idx: usize, x: i32, y: i32) {
    unsafe {
        let n = ICON_SIZE as usize;
        for row in 0..ICON_SIZE {
            for col in 0..ICON_SIZE {
                let c = ICON_CACHE[idx][row as usize * n + col as usize];
                if c != 0 {
                    put_pixel(x + col, y + row, c);
                }
            }
        }
    }
}

/// ColorOS 打断撕裂绘制：把缓存瓦片从中间撕成上下两半，各向对侧偏移
/// `split` 像素（半幅各 split/2），并按撕裂进度向白色淡出（飞得越远越淡）。
fn blit_icon_split(idx: usize, x: i32, y: i32, split: i32, fade: u32) {
    unsafe {
        let n = ICON_SIZE as usize;
        let mid = ICON_SIZE / 2;
        let dyu = split / 2;
        let dyd = (split + 1) / 2;
        for row in 0..mid {
            let py = y + row - dyu;
            if py < -4 || py >= DISPLAY_H as i32 + 4 {
                continue;
            }
            for col in 0..ICON_SIZE {
                let c = ICON_CACHE[idx][row as usize * n + col as usize];
                if c != 0 {
                    // 白色淡出时保留最高位字节的逐像素 alpha。
                    let out = if fade > 0 {
                        (c & 0xFF00_0000) | blend(c & 0x00_FF_FF_FF, 0x00_FF_FF_FF, fade)
                    } else {
                        c
                    };
                    put_pixel(x + col, py, out);
                }
            }
        }
        for row in mid..ICON_SIZE {
            let py = y + row + dyd;
            if py < -4 || py >= DISPLAY_H as i32 + 4 {
                continue;
            }
            for col in 0..ICON_SIZE {
                let c = ICON_CACHE[idx][row as usize * n + col as usize];
                if c != 0 {
                    // 白色淡出时保留最高位字节的逐像素 alpha。
                    let out = if fade > 0 {
                        (c & 0xFF00_0000) | blend(c & 0x00_FF_FF_FF, 0x00_FF_FF_FF, fade)
                    } else {
                        c
                    };
                    put_pixel(x + col, py, out);
                }
            }
        }
    }
}

fn draw_app_icon(app: &App, x: i32, y: i32, size: i32) {
    if size == ICON_SIZE {
        let idx = app_index_of(app);
        if idx < APPS.len() && unsafe { ICON_CACHE_READY[idx] } {
            blit_icon(idx, x, y);
            return;
        }
    }
    if draw_icon_asset(app, x, y, size) {
        return;
    }
    draw_icon_at(x, y, size, app.color);
    let white = 0x00_FF_FF_FF;
    let q = (size / 4).max(1);
    let h = (size / 2).max(2);
    match app.key {
        "phone" => {
            draw_line(x + q, y + q, x + size - q, y + size - q, white);
            fill_rect(x + q - 2, y + q - 1, 5, 8, white);
            fill_rect(x + size - q - 2, y + size - q - 6, 5, 8, white);
        }
        "sms" | "mail" => {
            draw_rect(x + q / 2, y + q, size - q, h, white);
            draw_line(x + q / 2, y + q, x + size / 2, y + q + h / 2, white);
            draw_line(
                x + size - q / 2 - 1,
                y + q,
                x + size / 2,
                y + q + h / 2,
                white,
            );
        }
        "contacts" => {
            fill_rect(x + size / 2 - 3, y + q, 6, 6, white);
            fill_rect(x + q, y + h, size - 2 * q, q, white);
        }
        "settings" => {
            draw_rect(x + q, y + q, size - 2 * q, size - 2 * q, white);
            fill_rect(x + size / 2 - 2, y + size / 2 - 2, 4, 4, app.color);
        }
        "files" => {
            fill_rect(x + q / 2, y + q, size - q, size - q - 2, white);
            fill_rect(x + q, y + q - 3, h, 4, white);
        }
        "browser" | "map" => {
            draw_rect(x + q, y + q, size - 2 * q, size - 2 * q, white);
            draw_line(x + size / 2, y + q, x + size / 2, y + size - q, white);
            draw_line(x + q, y + size / 2, x + size - q, y + size / 2, white);
        }
        "calculator" | "calc" => {
            draw_rect(x + q, y + q / 2, size - 2 * q, size - q, white);
            fill_rect(x + q + 3, y + q + 2, size - 2 * q - 6, 3, white);
            fill_rect(x + q + 3, y + h + 3, 3, 3, white);
            fill_rect(x + size - q - 6, y + h + 3, 3, 3, white);
        }
        "calendar" => {
            draw_rect(x + q, y + q, size - 2 * q, size - 2 * q, white);
            draw_line(x + q, y + h, x + size - q, y + h, white);
        }
        "camera" | "gallery" => {
            draw_rect(x + q / 2, y + q, size - q, h, white);
            fill_rect(x + size / 2 - 3, y + size / 2 - 3, 6, 6, white);
        }
        "clock" => {
            draw_rect(x + q, y + q, size - 2 * q, size - 2 * q, white);
            draw_line(x + size / 2, y + size / 2, x + size / 2, y + q + 2, white);
            draw_line(
                x + size / 2,
                y + size / 2,
                x + size - q - 2,
                y + size / 2,
                white,
            );
        }
        "music" => {
            draw_line(x + h, y + q, x + h, y + size - q, white);
            draw_line(x + h, y + q, x + size - q, y + q - 2, white);
            fill_rect(x + q, y + size - q - 3, q, q, white);
        }
        "weather" => {
            fill_rect(x + q, y + h, size - 2 * q, q, white);
            fill_rect(x + h - 3, y + q, 7, 7, white);
        }
        "store" => {
            draw_rect(x + q, y + q, size - 2 * q, size - q - 2, white);
            draw_line(x + q, y + q, x + size / 2, y + q - 5, white);
            draw_line(x + size - q, y + q, x + size / 2, y + q - 5, white);
        }
        "terminal" => {
            draw_line(x + q, y + q, x + h, y + h, white);
            draw_line(x + h, y + h, x + q, y + size - q, white);
            draw_line(x + h, y + size - q, x + size - q, y + size - q, white);
        }
        _ => fill_rect(x + q, y + q, size - 2 * q, size - 2 * q, white),
    }
}

fn time_hhmm(buf: &mut [u8; 5]) {
    let us = now_us();
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

// ==================== 桌面渲染 ====================

fn draw_status_bar() {
    // 液态玻璃状态栏：模糊壁纸 + 浅色着色 + 底部高光描边（深色文字/图标）。
    draw_glass(0, 0, DISPLAY_W as i32, STATUS_H, 0);
    // 左侧：时间
    let mut hhmm = [0u8; 5];
    time_hhmm(&mut hhmm);
    draw_text(8, (STATUS_H - 8) / 2, &hhmm, COLOR_TEXT_MAIN);
    // 右侧：WiFi + 电量
    unsafe {
        if WIFI_ENABLED {
            draw_wifi(486, (STATUS_H - 14) / 2, COLOR_TEXT_MAIN);
        } else {
            draw_line(486, 5, 500, 18, COLOR_WARN);
            draw_line(500, 5, 486, 18, COLOR_WARN);
        }
    }
    draw_battery(512, (STATUS_H - 10) / 2, COLOR_TEXT_MAIN);
    if unsafe { SHOW_BATTERY_PERCENT } {
        draw_text(532, 8, b"85", COLOR_TEXT_MAIN);
    }
}

fn draw_overlay() {
    unsafe {
        let now = now_us();
        if TOAST_UNTIL > now && !TOAST.is_empty() {
            let w = text_width(TOAST).min(420) + 24;
            let x = (DISPLAY_W as i32 - w) / 2;
            // Toast 显示在 Dock 与导航栏之上方，避免被遮挡。
            let base_ty = DOCK_Y - 96;
            // 入场 180ms：底部上滑 16px + 淡入（FastOutSlowIn）；
            // 结束前 150ms 淡出。保持期 PIX_ALPHA = 255，零混合开销。
            let e_in = if now > TOAST_T0 {
                Easing::FastOutSlowIn.sample((((now - TOAST_T0) as f32) / 180_000.0).min(1.0))
            } else {
                0.0
            };
            let out_left = TOAST_UNTIL.saturating_sub(now) as f32 / 150_000.0;
            let alpha = ((255.0 * e_in * out_left.min(1.0)) as u32).min(255);
            if alpha == 0 {
                return;
            }
            let ty = base_ty + (16.0 * (1.0 - e_in)) as i32;
            // 液态玻璃 Toast：圆角药丸 + 模糊背景（浅色玻璃 + 深字）。
            let prev = PIX_ALPHA;
            PIX_ALPHA = alpha;
            draw_glass(x, ty, w, 40, 20);
            draw_text(x + 12, ty + 16, TOAST, COLOR_TEXT_MAIN);
            PIX_ALPHA = prev;
        }
    }
}

fn draw_ripple() {
    unsafe {
        if !RIPPLE_ACTIVE {
            return;
        }
        let age = now_us().saturating_sub(RIPPLE_START);
        if age > 360_000 {
            RIPPLE_ACTIVE = false;
            return;
        }
        let p = (age as f32 / 360_000.0).min(1.0);
        // 半径 fast_out_slow_in（先快后慢扩散），透明度随 EaseIn 缓慢淡出，
        // 避免线性增长的生硬感（与 iOS/Android 水波动效对齐）
        let radius = 8 + (42.0 * Easing::FastOutSlowIn.sample(p) + 0.5) as i32;
        let alpha = (180.0 * (1.0 - Easing::EaseIn.sample(p)) + 0.5) as u32;
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                let d = isqrt(dx * dx + dy * dy);
                if d >= radius - 2 && d <= radius {
                    let px = (RIPPLE_X + dx).clamp(0, DISPLAY_W as i32 - 1) as usize;
                    let py = (RIPPLE_Y + dy).clamp(0, DISPLAY_H as i32 - 1) as usize;
                    let idx = py * DISPLAY_W + px;
                    BACK_BUF[idx] = blend(BACK_BUF[idx], COLOR_TEXT_MAIN, alpha);
                }
            }
        }
    }
}

/// 行优先自动布局：把一页的项目填入网格（不越界、不重叠）。
fn layout_page(items: &PageItems, cols: i32, out: &mut [PlacedItem; MAX_PAGE_ITEMS]) -> usize {
    let mut grid = [0u8; (GRID_ROWS as usize) * 8];
    let mut n = 0usize;
    for idx in 0..items.count {
        let (w, h) = item_size(items.items[idx].kind);
        'scan: for row in 0..=GRID_ROWS - h {
            for col in 0..=cols - w {
                let mut ok = true;
                for dy in 0..h {
                    for dx in 0..w {
                        if grid[(row + dy) as usize * 8 + (col + dx) as usize] != 0 {
                            ok = false;
                        }
                    }
                }
                if ok {
                    for dy in 0..h {
                        for dx in 0..w {
                            grid[(row + dy) as usize * 8 + (col + dx) as usize] = 1;
                        }
                    }
                    out[n] = PlacedItem {
                        index: idx,
                        col,
                        row,
                        w,
                        h,
                        kind: items.items[idx].kind,
                    };
                    n += 1;
                    break 'scan;
                }
            }
        }
    }
    n
}

fn layout_current() {
    unsafe {
        PLACED_COUNT = layout_page(&PAGES[CURRENT_PAGE], GRID_COLS, &mut PLACED);
    }
}

/// 项目尺寸（网格单元）。
fn item_size(kind: ItemKind) -> (i32, i32) {
    match kind {
        ItemKind::App(_) | ItemKind::Folder => (1, 1),
        ItemKind::Widget(WidgetKind::Clock) => (4, 2),
        ItemKind::Widget(WidgetKind::Weather) => (2, 1),
        ItemKind::Widget(WidgetKind::Calendar) => (2, 1),
        ItemKind::Widget(WidgetKind::Gallery) => (2, 2),
        ItemKind::Widget(WidgetKind::Battery) => (1, 1),
    }
}

/// 小组件圆角：小 12 / 中 16 / 大与超大 20。
fn widget_radius(w: i32, h: i32) -> i32 {
    if w == 1 {
        12
    } else if h == 1 {
        16
    } else {
        20
    }
}

/// 项目在屏幕上的矩形（应用/文件夹为图标区，小组件为整块）。
fn item_rect(p: &PlacedItem, cols: i32) -> (i32, i32, i32, i32) {
    let x = grid_x(cols) + p.col * CELL_W;
    let y = GRID_Y + p.row * CELL_H;
    match p.kind {
        ItemKind::App(_) | ItemKind::Folder => (x, y, ICON_SIZE, ICON_SIZE),
        ItemKind::Widget(_) => (
            x,
            y,
            p.w * CELL_W - ICON_H_GAP,
            p.h * CELL_H - LABEL_H - ICON_V_GAP,
        ),
    }
}

fn label_color() -> u32 {
    if unsafe { THEME_DARK } {
        COLOR_TEXT
    } else {
        // 浅色主题：白底/浅壁纸上深色标签。
        COLOR_TEXT_MAIN
    }
}

fn folder_name_bytes() -> &'static [u8] {
    unsafe {
        if FOLDER_NAME_LEN > 0 {
            core::slice::from_raw_parts(FOLDER_NAME.as_ptr(), FOLDER_NAME_LEN)
        } else if LANG_ZH {
            b"\xE7\xB3\xBB\xE7\xBB\x9F\xE5\xB7\xA5\xE5\x85\xB7"
        } else {
            b"System Tools"
        }
    }
}

/// 应用图标 + 单行标签（12px，超出截断）。
fn draw_app_item(app_idx: usize, item_idx: usize, x: i32, y: i32) {
    let app = &APPS[app_idx];
    unsafe {
        let (size, ox, oy) = pressed_icon_rect(app.key);
        if EDIT_MODE {
            let t = now_us() / 150_000;
            let phase = (item_idx as u64) * 5;
            let ang = SINE16[(t + phase) as usize % 16] as f32 / 100.0 * 2.0;
            draw_icon_rot(app, x + ICON_SIZE / 2, y + ICON_SIZE / 2, size, ang, 1.0);
            fill_round_rect(x + size - 10, y - 6, 18, 18, 9, 0x00_DC_26_26);
            draw_text_px(x + size - 5, y - 2, b"-", 13.0, COLOR_TEXT);
        } else {
            draw_app_icon(app, x + ox, y + oy, size);
        }
    }
    draw_app_label(x, y, app.display_name().as_bytes());
}

fn draw_folder_item(x: i32, y: i32, item_idx: usize) {
    unsafe {
        if EDIT_MODE {
            let t = now_us() / 150_000;
            let phase = (item_idx as u64) * 5;
            let ang = SINE16[(t + phase) as usize % 16] as f32 / 100.0 * 2.0;
            draw_icon_rot_folder(x + ICON_SIZE / 2, y + ICON_SIZE / 2, ang);
        } else {
            draw_folder_icon(x, y, ICON_SIZE);
        }
    }
    draw_app_label(x, y, folder_name_bytes());
}

fn draw_app_label(x: i32, y: i32, name: &[u8]) {
    let w = text_width_px(name, 12.0);
    if w > CELL_W {
        draw_label_trunc(
            x + (ICON_SIZE - CELL_W) / 2,
            y + ICON_SIZE + 2,
            name,
            12.0,
            CELL_W,
            label_color(),
        );
    } else {
        draw_text_px(
            x + (ICON_SIZE - w) / 2,
            y + ICON_SIZE + 2,
            name,
            12.0,
            label_color(),
        );
    }
}

/// 文件夹图标：圆角矩形 + 四个迷你图标（代表内容）。
fn draw_folder_icon(x: i32, y: i32, size: i32) {
    let r = icon_radius(size).max(1);
    fill_round_rect(x, y, size, size, r, COLOR_FOLDER);
    let m = (size / 5).max(4);
    let gap = m / 2;
    let ox = x + size / 2 - m - gap / 2;
    let oy = y + size / 2 - m - gap / 2;
    for i in 0..4 {
        let mx = ox + (i % 2) as i32 * (m + gap);
        let my = oy + (i / 2) as i32 * (m + gap);
        fill_round_rect(mx, my, m, m, m / 3, 0x00_FF_EF_C8);
    }
    draw_round_rect(x, y, size, size, r, COLOR_ICON_BORDER);
}

/// 把图标渲染到瓦片后按角度旋转绘制（编辑模式抖动，2°，150ms 周期）。
fn draw_icon_rot(app: &App, cx: i32, cy: i32, size: i32, ang_deg: f32, scale: f32) {
    unsafe {
        TILE_MODE = true;
        draw_app_icon(app, 0, 0, size);
        TILE_MODE = false;
    }
    let rad = ang_deg * PI / 180.0;
    let cos = fcos(rad);
    let sin = fsin(rad);
    let half = (size as f32 * scale * 0.5) as i32;
    let mid = size as f32 * 0.5;
    for py in -half..=half {
        for px in -half..=half {
            let sx = (px as f32 * cos + py as f32 * sin + mid) as i32;
            let sy = (-px as f32 * sin + py as f32 * cos + mid) as i32;
            if sx >= 0 && sy >= 0 && sx < size && sy < size {
                let c = unsafe { ICON_TILE[sy as usize * size as usize + sx as usize] };
                if c != 0 {
                    put_pixel(cx + px, cy + py, c);
                }
            }
        }
    }
}

fn draw_icon_rot_folder(cx: i32, cy: i32, ang_deg: f32) {
    unsafe {
        TILE_MODE = true;
        draw_folder_icon(0, 0, ICON_SIZE);
        TILE_MODE = false;
    }
    let rad = ang_deg * PI / 180.0;
    let cos = fcos(rad);
    let sin = fsin(rad);
    let half = ICON_SIZE / 2;
    let n = ICON_SIZE as usize;
    for py in -half..=half {
        for px in -half..=half {
            let sx = (px as f32 * cos + py as f32 * sin + ICON_SIZE as f32 * 0.5) as i32;
            let sy = (-px as f32 * sin + py as f32 * cos + ICON_SIZE as f32 * 0.5) as i32;
            if sx >= 0 && sy >= 0 && sx < ICON_SIZE && sy < ICON_SIZE {
                let c = unsafe { ICON_TILE[sy as usize * n + sx as usize] };
                if c != 0 {
                    put_pixel(cx + px, cy + py, c);
                }
            }
        }
    }
}

fn draw_page() {
    unsafe {
        if PAGE_ANIM || PAGE_DRAG {
            // 翻页动画：第 k 页绘制在 offset + k*屏宽（ColorOS 15 页面滑动）。
            // 窗口取 ±2 页：快速连续翻页时偏移可瞬时超过一屏，
            // 保证可视区域始终有页面覆盖；出屏的页由 draw_page_at 跳过。
            let w = DISPLAY_W as i32;
            let off = PAGE_OFFSET as i32;
            let cur = CURRENT_PAGE as i32;
            let mut k = -2i32;
            while k <= 2 {
                let p = cur + k;
                if p >= 0 && (p as usize) < page_count() {
                    draw_page_at(p as usize, off + k * w);
                }
                k += 1;
            }
        } else {
            draw_page_at(CURRENT_PAGE, 0);
        }
        if EDIT_MODE {
            draw_edit_hud();
        }
        if DRAG_ACTIVE && DRAG_APP != usize::MAX {
            draw_dragged_icon();
        }
        if !BOOT_ACTIVE {
            draw_page_dots();
        }
    }
}

/// 绘制某一页的内容（items 整体水平偏移 `offset` 像素）。
/// 翻页动画期间相邻页各按 ±屏宽 排布；整页出屏时直接跳过。
/// 逐像素绘制经 put_pixel 边界剪裁，部分可见的图标/小组件无需特殊处理。
fn draw_page_at(page: usize, offset: i32) {
    if offset <= -(DISPLAY_W as i32) || offset >= DISPLAY_W as i32 {
        return;
    }
    unsafe {
        let cols = GRID_COLS;
        let mut placed = [PlacedItem::zero(); MAX_PAGE_ITEMS];
        let n = layout_page(&PAGES[page], cols, &mut placed);
        for i in 0..n {
            // 渐进式启动：应用/文件夹图标按 BOOT_LIMIT 分批出现，小组件立即绘制。
            if BOOT_ACTIVE && i >= BOOT_LIMIT {
                match placed[i].kind {
                    ItemKind::App(_) | ItemKind::Folder => continue,
                    _ => {}
                }
            }
            let p = placed[i];
            let (mut px, py, pw, ph) = item_rect(&p, cols);
            px += offset;
            if px + pw <= 0 || px >= DISPLAY_W as i32 {
                continue; // 整项出屏
            }
            match p.kind {
                ItemKind::App(app) => {
                    if app_is_flying(app) {
                        continue;
                    }
                    if DRAG_ACTIVE && DRAG_ITEM == p.index {
                        continue; // 拖拽中隐藏原位
                    }
                    draw_app_item(app, p.index, px, py);
                }
                ItemKind::Folder => {
                    draw_folder_item(px, py, p.index);
                }
                ItemKind::Widget(k) => {
                    let (wc, hc) = item_size(p.kind);
                    draw_widget(k, px, py, pw, ph, wc, hc);
                }
            }
        }
    }
}

fn draw_edit_hud() {
    unsafe {
        let hint = txt(
            "Edit: drag to move, tap to remove",
            "编辑模式：拖动移动，点击移除",
        );
        draw_text_px(16, 36, hint, 12.0, COLOR_TEXT_DIM);
        // 网格切换按钮（4x6 / 5x6）—— 液态玻璃（浅色玻璃 + 深字）。
        let bx = DISPLAY_W as i32 - 128;
        let by = 32;
        draw_glass(bx, by, 112, 34, 16);
        let label = if GRID_COLS == 4 {
            txt("Grid 5x6", "网格 5x6")
        } else {
            txt("Grid 4x6", "网格 4x6")
        };
        let lw = text_width_px(label, 12.0);
        draw_text_px(bx + (112 - lw) / 2, by + 11, label, 12.0, COLOR_TEXT_MAIN);
    }
}

fn grid_toggle_hit(x: i32, y: i32) -> bool {
    let bx = DISPLAY_W as i32 - 128;
    x >= bx && x < bx + 112 && y >= 32 && y < 66
}

fn toggle_grid() {
    unsafe {
        GRID_COLS = if GRID_COLS == 4 { 5 } else { 4 };
        save_number(GRID_PATH, GRID_COLS as u32);
        let label = if GRID_COLS == 5 {
            txt("Grid 5x6", "网格 5x6")
        } else {
            txt("Grid 4x6", "网格 4x6")
        };
        show_toast(label);
        NEEDS_REDRAW = true;
    }
}

/// 拖拽图标：跟随手指，钳制在网格区域内（不越界）。
fn draw_dragged_icon() {
    unsafe {
        let app = &APPS[DRAG_APP];
        let cols = GRID_COLS;
        let gx0 = grid_x(cols);
        let gy0 = GRID_Y;
        let gx1 = gx0 + cols * CELL_W - ICON_H_GAP;
        let gy1 = gy0 + GRID_ROWS * CELL_H;
        let half = (ICON_SIZE as f32 * 1.12 * 0.5) as i32;
        let cx = DRAG_X.clamp(gx0 + half, gx1 - half);
        let cy = DRAG_Y.clamp(gy0 + half, gy1 - half);
        draw_glass_solid(
            cx - half + 4,
            cy - half + 6,
            ICON_SIZE,
            ICON_SIZE,
            icon_radius(ICON_SIZE),
            0x00_00_00_00,
            70,
            0x00_00_00_00,
        );
        draw_icon_rot(app, cx, cy, ICON_SIZE, 0.0, 1.12);
    }
}

fn draw_page_dots() {
    let pages = page_count();
    if pages <= 1 {
        return;
    }
    unsafe {
        let gap = 16;
        let start = DISPLAY_W as i32 / 2 - ((pages as i32 - 1) * gap) / 2;
        // 翻页进度（0..1）：页码点整体随偏移平移，当前页与目标页的颜色
        // 按进度交叉过渡（ColorOS 指示点跟随手感）。
        let t = (PAGE_OFFSET.abs() / DISPLAY_W as f32).min(1.0);
        let shift = (PAGE_OFFSET / DISPLAY_W as f32) * gap as f32;
        let dir = if PAGE_OFFSET > 0.0 { -1i32 } else { 1 };
        let other = CURRENT_PAGE as i32 + dir;
        let active = COLOR_TEXT_MAIN;
        let idle = 0x00_78_85_98;
        for i in 0..pages {
            let mut c = if i == CURRENT_PAGE { active } else { idle };
            if t > 0.0 && other >= 0 && (other as usize) < pages {
                let a = (t * 255.0) as u32;
                if i as i32 == other {
                    c = blend(idle, active, a);
                } else if i == CURRENT_PAGE {
                    c = blend(active, idle, a);
                }
            }
            fill_rect(
                start + i as i32 * gap - 3 + shift as i32,
                DOCK_Y - 24,
                7,
                7,
                c,
            );
        }
    }
}

// ==================== 翻页动画控制 ====================

/// 当前是否允许进入跟手翻页（空闲桌面上的横向拖动）。
fn page_drag_allowed() -> bool {
    unsafe {
        !LOCKED
            && !PAGE_DRAG
            && !PAGE_ANIM
            && !RECENTS_OPEN
            && !EDIT_MODE
            && !DRAG_ACTIVE
            && !RENAME_OPEN
            && FOLDER_PHASE == FolderPhase::Closed
            && !control_center::cc_visible()
            && !control_center::shade_drag_active()
            && !BOOT_ACTIVE
            && page_count() > 0
            && matches!(MOTION.phase, Phase::Idle | Phase::PressIn)
    }
}

/// 跟手翻页：把本轮手势的水平位移映射为页面偏移（页界外 0.35x 橡皮筋）。
fn page_drag_update(dx: i32) {
    unsafe {
        let w = DISPLAY_W as f32;
        let o = dx as f32;
        PAGE_OFFSET = if o > w {
            w + (o - w) * 0.35
        } else if o < -w {
            -w + (o + w) * 0.35
        } else {
            o
        };
        NEEDS_REDRAW = true;
    }
}

/// 结算翻页：`dir` = 1 下一页 / -1 上一页 / 0 回弹；`vel` 为手势水平速度
/// （px/s，带入弹簧作为初速度，快扫轻拨也能翻页）。
fn page_settle(dir: i32, vel: f32) {
    unsafe {
        PAGE_DRAG = false;
        PAGE_TRACK = false;
        if page_count() > 1 && dir != 0 {
            let np = (CURRENT_PAGE as i32 + dir).clamp(0, page_count() as i32 - 1);
            if np as usize != CURRENT_PAGE {
                // 提交目标页：新当前页从原相邻页位置（±屏宽处）弹入原位，
                // 与跟手段的页面位置无缝衔接。
                let w = DISPLAY_W as f32;
                PAGE_OFFSET = if dir > 0 {
                    PAGE_OFFSET + w
                } else {
                    PAGE_OFFSET - w
                };
                CURRENT_PAGE = np as usize;
            }
        }
        PAGE_ANIM = true;
        PAGE_SPRING = Spring::page();
        // 弹簧从当前偏移（翻页时为 ±屏宽附近，回弹时为拖动残位）弹回 0；
        // 不初始化 pos 的话第一帧 PAGE_OFFSET 直接跳到弹簧 pos（≈0），页面瞬移。
        PAGE_SPRING.pos = PAGE_OFFSET;
        PAGE_SPRING.target = 0.0;
        PAGE_SPRING.vel = vel.clamp(-4000.0, 4000.0);
        PAGE_SPRING_T = now_us();
        NEEDS_REDRAW = true;
    }
}

/// 翻页弹簧推进：返回 true 表示动画仍在进行（需要继续重绘）。
fn page_advance(now: u64) -> bool {
    unsafe {
        if !PAGE_ANIM {
            return false;
        }
        let dt = now.saturating_sub(PAGE_SPRING_T) as f32 / 1000.0;
        PAGE_SPRING_T = now;
        if PAGE_SPRING.step_ms(dt) {
            PAGE_OFFSET = 0.0;
            PAGE_ANIM = false;
            NEEDS_REDRAW = true;
            return false;
        }
        PAGE_OFFSET = PAGE_SPRING.pos;
        true
    }
}

/// 按压反馈几何：返回 (图标边长, x 偏移, y 偏移)；无按压返回原始尺寸。
fn pressed_icon_rect(key: &'static str) -> (i32, i32, i32) {
    let ps = pressed_scale_for_app(key);
    match ps {
        Some(scale) => {
            let size = ((ICON_SIZE as u32 * scale) / SCALE_1).max(1) as i32;
            let o = (ICON_SIZE - size) / 2;
            (size, o, o)
        }
        None => (ICON_SIZE, 0, 0),
    }
}

/// 指定应用（key）是否处于按压反馈态。
fn pressed_scale_for_app(key: &'static str) -> Option<u32> {
    unsafe {
        if MOTION.press_app == usize::MAX {
            return None;
        }
        if APPS[MOTION.press_app].key != key {
            return None;
        }
        match MOTION.phase {
            Phase::PressIn | Phase::PressOut => Some(MOTION.press_scale),
            _ => None,
        }
    }
}

fn dock_rect(i: usize) -> (i32, i32) {
    unsafe {
        let total_w = DOCK_COUNT as i32 * DOCK_ICON + (DOCK_COUNT as i32 - 1) * DOCK_GAP;
        let start_x = (DISPLAY_W as i32 - total_w) / 2;
        let dy = DOCK_Y + (DOCK_H - DOCK_ICON) / 2;
        (start_x + i as i32 * (DOCK_ICON + DOCK_GAP), dy)
    }
}

fn draw_dock() {
    // 悬浮液态玻璃 Dock：圆角药丸（模糊壁纸 + 浅色着色 + 顶部高光描边），
    // 图标仍绘制在原有 dock_rect 位置上，命中逻辑不变。
    draw_glass(12, DOCK_Y + 2, DISPLAY_W as i32 - 24, DOCK_H - 4, 30);
    unsafe {
        let limit = if BOOT_ACTIVE { BOOT_LIMIT } else { usize::MAX };
        for i in 0..DOCK_COUNT {
            if i >= limit {
                break;
            }
            let app_idx = DOCK_APPS[i];
            // 打断动画中：该应用图标正在飞行，隐藏 Dock 原位。
            if app_is_flying(app_idx) {
                continue;
            }
            let app = &APPS[app_idx];
            let (dx, dy) = dock_rect(i);
            let (size, ox, oy) = pressed_icon_rect(app.key);
            draw_app_icon(app, dx + ox, dy + oy, size);
        }
    }
}

/// 渐进式启动的加载指示器（位于 Dock 上方）：进度条 + 百分比。
fn draw_boot_progress() {
    unsafe {
        if !BOOT_ACTIVE {
            return;
        }
        let total = BOOT_TOTAL.max(1);
        let pct = BOOT_LIMIT.min(total) * 100 / total;
        let bw = 240i32;
        let bx = (DISPLAY_W as i32 - bw) / 2;
        let by = DOCK_Y - 56;
        // 液态玻璃启动进度面板（浅色玻璃 + 深字，浅灰轨道 + 强调绿进度）。
        draw_glass(bx - 10, by, bw + 20, 52, 16);
        let label = txt("LOADING", "正在进入桌面");
        let lw = text_width(label);
        draw_text(bx + (bw - lw) / 2, by + 4, label, COLOR_TEXT_MAIN);
        fill_rect(bx, by + 26, bw, 10, COLOR_DIVIDER);
        fill_rect(bx, by + 26, (bw * pct as i32) / 100, 10, COLOR_ACCENT);
        let mut ps = [0u8; 4];
        let pn = push_dec(&mut ps, pct as u32);
        ps[pn] = b'%';
        let pw = text_width(&ps[..pn + 1]);
        draw_text(bx + bw - pw, by + 38, &ps[..pn + 1], COLOR_TEXT_MAIN);
    }
}

// ==================== 三角函数（泰勒级数） ====================

const PI: f32 = 3.14159265;
const TAU: f32 = 6.28318530;

fn fsin(mut x: f32) -> f32 {
    x = x % TAU;
    if x > PI {
        x -= TAU;
    }
    if x < -PI {
        x += TAU;
    }
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let a = x.abs();
    let q = (a / (PI / 2.0)) as i32 % 4;
    let t = a - q as f32 * (PI / 2.0);
    let s = t - t * t * t / 6.0 + t * t * t * t * t / 120.0 - t * t * t * t * t * t * t / 5040.0
        + t * t * t * t * t * t * t * t * t / 362880.0;
    let c = 1.0 - t * t / 2.0 + t * t * t * t / 24.0 - t * t * t * t * t * t / 720.0
        + t * t * t * t * t * t * t * t / 40320.0;
    match q {
        0 => sign * s,
        1 => sign * c,
        2 => -sign * s,
        _ => -sign * c,
    }
}

fn fcos(x: f32) -> f32 {
    fsin(x + PI / 2.0)
}

// ==================== 小组件 ====================

/// 3-tap 盒式模糊（水平 + 垂直两趟），就地操作。
fn blur_region(buf: &mut [u32], bw: usize, bh: usize, x: usize, y: usize, w: usize, h: usize) {
    for dy in y..y + h {
        for dx in x..x + w {
            let i = dy * bw + dx;
            let c0 = buf[dy * bw + dx.saturating_sub(1)];
            let c1 = buf[i];
            let c2 = buf[dy * bw + (dx + 1).min(bw - 1)];
            buf[i] = avg3(c0, c1, c2);
        }
    }
    for dx in x..x + w {
        for dy in y..y + h {
            let i = dy * bw + dx;
            let c0 = buf[dy.saturating_sub(1) * bw + dx];
            let c1 = buf[i];
            let c2 = buf[(dy + 1).min(bh - 1) * bw + dx];
            buf[i] = avg3(c0, c1, c2);
        }
    }
}

/// 半透明毛玻璃圆角背景：原图保存 → 模糊 → 玻璃合成（圆角外恢复原样）。
fn draw_frost_round_rect(x: i32, y: i32, w: i32, h: i32, r: i32) {
    let wu = w as usize;
    let hu = h as usize;
    for dy in 0..hu {
        for dx in 0..wu {
            let sx = x + dx as i32;
            let sy = y + dy as i32;
            let c = if sx >= 0 && sy >= 0 && sx < DISPLAY_W as i32 && sy < DISPLAY_H as i32 {
                unsafe { BACK_BUF[sy as usize * DISPLAY_W + sx as usize] }
            } else {
                0
            };
            unsafe {
                WIDGET_SCRATCH[dy * WIDGET_SCRATCH_W + dx] = c;
            }
        }
    }
    unsafe {
        blur_region(
            &mut BACK_BUF,
            DISPLAY_W,
            DISPLAY_H,
            x as usize,
            y as usize,
            wu,
            hu,
        );
    }
    for dy in 0..hu {
        for dx in 0..wu {
            let idx = (y + dy as i32) as usize * DISPLAY_W + (x + dx as i32) as usize;
            let orig = unsafe { WIDGET_SCRATCH[dy * WIDGET_SCRATCH_W + dx] };
            // 覆盖度：仅四个圆角盒超采样，内部恒为不透明（抗锯齿轮廓）。
            let cov = if (dx < r as usize || dx >= w as usize - r as usize)
                && (dy < r as usize || dy >= h as usize - r as usize)
            {
                glass::rr_coverage(dx as f32, dy as f32, w, h, r)
            } else {
                255
            };
            if cov > 0 {
                let blurred = unsafe { BACK_BUF[idx] };
                let t = blend(orig, blurred, 170);
                // 浅色磨砂：白色着色为主，轻微冷灰避免过曝。
                let d = blend(t, 0x00_FF_FF_FF, 200);
                let d = blend(d, COLOR_DIVIDER, 60);
                unsafe { BACK_BUF[idx] = blend(orig, d, cov) };
            } else {
                unsafe {
                    BACK_BUF[idx] = orig;
                }
            }
        }
    }
    draw_round_rect(x, y, w, h, r, COLOR_BORDER);
}

/// 编辑模式下的小组件背景（实心半透明，避免每帧模糊开销）。
/// `border` 为 1px 圆角描边色（浅色面板用 0xD9DEE6，投影用同色消隐）。
fn draw_glass_solid(x: i32, y: i32, w: i32, h: i32, r: i32, color: u32, alpha: u32, border: u32) {
    for dy in 0..h {
        for dx in 0..w {
            let cov = if (dx < r || dx >= w - r) && (dy < r || dy >= h - r) {
                glass::rr_coverage(dx as f32, dy as f32, w, h, r)
            } else {
                255
            };
            if cov > 0 {
                let sx = x + dx;
                let sy = y + dy;
                if sx >= 0 && sy >= 0 && sx < DISPLAY_W as i32 && sy < DISPLAY_H as i32 {
                    let idx = sy as usize * DISPLAY_W + sx as usize;
                    unsafe {
                        BACK_BUF[idx] = blend(BACK_BUF[idx], color, alpha * cov / 255);
                    }
                }
            }
        }
    }
    draw_round_rect(x, y, w, h, r, border);
}

/// 毛玻璃目标：背景取自 BACK_BUF（文件夹瓦片模式下即已压暗的桌面），
/// 像素经 put_pixel 写入（瓦片模式自动路由到 FOLDER_TILE）。
struct LauncherGlass;
impl GlassTarget for LauncherGlass {
    fn backdrop(&self, x: i32, y: i32) -> u32 {
        if x >= 0 && y >= 0 && x < DISPLAY_W as i32 && y < DISPLAY_H as i32 {
            unsafe { BACK_BUF[y as usize * DISPLAY_W + x as usize] }
        } else {
            0
        }
    }
    fn put(&mut self, x: i32, y: i32, color: u32) {
        put_pixel(x, y, color);
    }
}

/// 毛玻璃面板（浅色玻璃，THEME_DARK = false 桌面风格）。
fn draw_glass(x: i32, y: i32, w: i32, h: i32, r: i32) {
    glass_rect(
        &mut LauncherGlass,
        x,
        y,
        w,
        h,
        r,
        unsafe { THEME_DARK },
        true,
    );
}

/// 自定义样式液态玻璃面板。
fn draw_glass_style(x: i32, y: i32, w: i32, h: i32, r: i32, st: &GlassStyle) {
    glass_rect_style(&mut LauncherGlass, x, y, w, h, r, st, true);
}

/// 玻璃按键（圆角玻璃方块，锁屏/重命名键盘用）。
fn draw_glass_key(x: i32, y: i32, w: i32, h: i32) {
    let r = (h * 25 / 100).min(12);
    draw_glass(x, y, w, h, r);
}

fn fill_vgrad_masked(x: i32, y: i32, w: i32, h: i32, r: i32, top: u32, bottom: u32) {
    for dy in 0..h {
        let t = ((dy as u32 * 255) / h.max(1) as u32).min(255);
        let row = blend(top, bottom, t);
        for dx in 0..w {
            if in_round_rect(dx, dy, w, h, r) {
                put_pixel(x + dx, y + dy, row);
            }
        }
    }
}

/// 星期（0=周日），1970-01-01 为周四。
fn weekday(days: i64) -> usize {
    ((((days + 4) % 7) + 7) % 7) as usize
}

const WEEK_ZH: [&str; 7] = ["日", "一", "二", "三", "四", "五", "六"];
const WEEK_EN: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTH_EN: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn weekday_label(wd: usize) -> &'static [u8] {
    if unsafe { LANG_ZH } {
        unsafe {
            if WEEK_BUF_LEN != wd + 1 {
                WEEK_BUF[..3].copy_from_slice(b"\xE5\x91\xA8");
                WEEK_BUF[3..].copy_from_slice(WEEK_ZH[wd].as_bytes());
                WEEK_BUF_LEN = wd + 1;
            }
            &WEEK_BUF[..6]
        }
    } else {
        WEEK_EN[wd].as_bytes()
    }
}

/// 时钟小组件（4x2）：表盘 + 指针（秒针实时更新）+ 数字时钟 + 日期。
fn draw_widget_clock(x: i32, y: i32, _w: i32, h: i32) {
    let cx = x + 72;
    let cy = y + h / 2;
    let r = ((h / 2 - 10).min(62)).max(20);
    // 表盘双环：外圈浅灰描边 + 内圈深色主环（浅色玻璃上）。
    draw_circle(cx, cy, r, COLOR_BORDER);
    draw_circle(cx, cy, r - 1, COLOR_TEXT_MAIN);
    for i in 0..12 {
        let a = i as f32 * PI / 6.0;
        let s = fsin(a);
        let c = fcos(a);
        let x1 = cx + (s * (r - 9) as f32) as i32;
        let y1 = cy - (c * (r - 9) as f32) as i32;
        let x2 = cx + (s * r as f32) as i32;
        let y2 = cy - (c * r as f32) as i32;
        draw_line(x1, y1, x2, y2, COLOR_TEXT_DIM);
    }
    let us = now_us();
    let secs = (us / 1_000_000) as u32;
    let total_min = secs / 60;
    let hh = (total_min / 60) % 24;
    let mm = total_min % 60;
    let ss = secs % 60;
    let ha = ((hh % 12) as f32 + mm as f32 / 60.0) * 30.0 * PI / 180.0;
    let ma = (mm as f32 + ss as f32 / 60.0) * 6.0 * PI / 180.0;
    let sa = ss as f32 * 6.0 * PI / 180.0;
    draw_hand(cx, cy, ha, r * 5 / 10, 3, COLOR_TEXT_MAIN);
    draw_hand(cx, cy, ma, r * 75 / 100, 2, COLOR_TEXT_MAIN);
    draw_hand(cx, cy, sa, r * 88 / 100, 1, 0x00_E7_4C_3C);
    fill_rect(cx - 2, cy - 2, 4, 4, COLOR_TEXT_MAIN);
    // 右侧：数字时钟（HH:MM，8x8 字形 2 倍）+ 日期 + 星期
    let dx0 = cx + r + 16;
    let mut hhmm = [0u8; 5];
    time_hhmm(&mut hhmm);
    draw_big_glyphs(dx0, y + 30, &hhmm, 2, COLOR_TEXT_MAIN);
    let days = secs as i64 / 86400;
    let (_, mo, dd) = civil_from_days(days);
    let wd = weekday(days);
    let mut ds = [0u8; 24];
    let mut n = 0;
    if unsafe { LANG_ZH } {
        n += push_dec(&mut ds[n..], mo);
        ds[n..n + 3].copy_from_slice(b"\xE6\x9C\x88");
        n += 3;
        n += push_dec(&mut ds[n..], dd);
        ds[n..n + 3].copy_from_slice(b"\xE6\x97\xA5");
        n += 3;
        ds[n..n + 6].copy_from_slice(weekday_label(wd));
        n += 6;
    } else {
        ds[n..n + 3].copy_from_slice(MONTH_EN[(mo - 1) as usize].as_bytes());
        n += 3;
        ds[n] = b' ';
        n += 1;
        n += push_dec(&mut ds[n..], dd);
        ds[n] = b' ';
        n += 1;
        ds[n..n + 3].copy_from_slice(WEEK_EN[wd].as_bytes());
        n += 3;
    }
    draw_text_px(dx0, y + 30 + 2 * 8 + 10, &ds[..n], 13.0, COLOR_TEXT_DIM);
}

fn draw_hand(cx: i32, cy: i32, ang: f32, len: i32, width: i32, color: u32) {
    let ex = cx + (fsin(ang) * len as f32) as i32;
    let ey = cy - (fcos(ang) * len as f32) as i32;
    for i in 0..width {
        let off = i as f32 - (width as f32 - 1.0) / 2.0;
        let ox = (fcos(ang) * off) as i32;
        let oy = (fsin(ang) * off) as i32;
        draw_line(cx + ox, cy + oy, ex + ox, ey + oy, color);
    }
}

#[derive(Clone, Copy, PartialEq)]
enum CondIcon {
    Sunny,
    Cloudy,
    Rainy,
    Snowy,
}

fn cond_icon(cond: &[u8]) -> CondIcon {
    if bytes_contains(cond, b"rain") || bytes_contains(cond, b"\xE9\x9B\xA8") {
        CondIcon::Rainy
    } else if bytes_contains(cond, b"snow") || bytes_contains(cond, b"\xE9\x9B\xAA") {
        CondIcon::Snowy
    } else if bytes_contains(cond, b"cloud") || bytes_contains(cond, b"\xE4\xBA\x91") {
        CondIcon::Cloudy
    } else {
        CondIcon::Sunny
    }
}

fn cond_label(cond: &[u8]) -> &'static [u8] {
    match cond_icon(cond) {
        CondIcon::Rainy => {
            if unsafe { LANG_ZH } {
                b"\xE5\xB0\x8F\xE9\x9B\xA8"
            } else {
                b"Rain"
            }
        }
        CondIcon::Snowy => {
            if unsafe { LANG_ZH } {
                b"\xE5\xB0\x8F\xE9\x9B\xAA"
            } else {
                b"Snow"
            }
        }
        CondIcon::Cloudy => {
            if unsafe { LANG_ZH } {
                b"\xE5\xA4\x9A\xE4\xBA\x91"
            } else {
                b"Cloudy"
            }
        }
        CondIcon::Sunny => {
            if unsafe { LANG_ZH } {
                b"\xE6\x99\xB4"
            } else {
                b"Sunny"
            }
        }
    }
}

fn draw_cloud(cx: i32, cy: i32, color: u32) {
    draw_circle(cx - 9, cy, 8, color);
    draw_circle(cx + 2, cy - 4, 10, color);
    draw_circle(cx + 12, cy, 7, color);
    fill_rect(cx - 9, cy, 21, 7, color);
}

fn draw_cond_icon(cond: &[u8], x: i32, y: i32) {
    let cx = x + 20;
    let cy = y + 18;
    match cond_icon(cond) {
        CondIcon::Sunny => {
            draw_circle(cx, cy, 9, 0x00_FF_D0_4D);
            for i in 0..8 {
                let a = i as f32 * PI / 4.0;
                let s = fsin(a);
                let c = fcos(a);
                draw_line(
                    cx + (s * 13.0) as i32,
                    cy - (c * 13.0) as i32,
                    cx + (s * 18.0) as i32,
                    cy - (c * 18.0) as i32,
                    0x00_FF_D0_4D,
                );
            }
        }
        CondIcon::Cloudy => {
            draw_cloud(cx, cy, 0x00_B9_C4_D4);
        }
        CondIcon::Rainy => {
            draw_cloud(cx, cy - 4, 0x00_8F_A3_BF);
            draw_line(cx - 8, cy + 8, cx - 12, cy + 16, 0x00_8F_C8_F0);
            draw_line(cx + 2, cy + 8, cx - 2, cy + 16, 0x00_8F_C8_F0);
            draw_line(cx + 12, cy + 8, cx + 8, cy + 16, 0x00_8F_C8_F0);
        }
        CondIcon::Snowy => {
            draw_cloud(cx, cy - 4, 0x00_B9_C4_D4);
            for i in 0..3 {
                draw_circle(cx - 8 + i * 8, cy + 12, 1, 0x00_9A_A7_B5);
            }
        }
    }
}

/// 天气小组件（2x1）：当前温度 + 天气图标 + 天气文字。
static mut WGT_COND_BUF: [u8; 16] = [0; 16];
static mut WGT_COND_LEN: usize = 0;
static mut WGT_COND_EXP: u64 = 0;

fn draw_widget_weather(x: i32, y: i32, w: i32, h: i32) {
    // 温度/天气文字走 1s TTL 缓存（动画期每帧重绘不再读盘）。
    let temp = read_number_cached(0, WEATHER_TEMP_PATH, 24);
    unsafe {
        let now = now_us();
        if now >= WGT_COND_EXP {
            WGT_COND_EXP = now + 1_000_000;
            WGT_COND_LEN = 0;
            let mut cond = [0u8; 16];
            let cn = read_file(WEATHER_COND_PATH, &mut cond);
            if cn > 0 {
                let cn = (cn as usize).min(WGT_COND_BUF.len());
                WGT_COND_BUF[..cn].copy_from_slice(&cond[..cn]);
                WGT_COND_LEN = cn;
            }
        }
    }
    let cond_s: &[u8] = unsafe {
        if WGT_COND_LEN > 0 {
            &WGT_COND_BUF[..WGT_COND_LEN]
        } else {
            b"sunny"
        }
    };
    // ★ 钳制温度到 3 位数：tb[10] 之后还要放 2 字节「°」(0xC2 0xB0)。
    // temp 来自文件解析（上限 u32::MAX，10 位数字），无钳制时 tb[n]/tb[n+1]
    // 索引越界 panic（launcher 崩溃 = 桌面崩溃）。
    let temp = temp.min(999);
    let mut tb = [0u8; 10];
    let mut n = push_dec(&mut tb, temp);
    tb[n] = 0xC2;
    tb[n + 1] = 0xB0;
    n += 2;
    draw_text_px(x + 12, y + 10, &tb[..n], 26.0, COLOR_TEXT_MAIN);
    draw_text_px(x + 12, y + 40, cond_label(cond_s), 12.0, COLOR_TEXT_DIM);
    draw_cond_icon(cond_s, x + w - 46, y + (h - 44) / 2);
}

/// 日历小组件（2x1）：日期 + 下一个事件。
fn draw_widget_calendar(x: i32, y: i32, w: i32, h: i32) {
    let us = now_us();
    let secs = (us / 1_000_000) as i64;
    let days = secs / 86400;
    let (_, mo, dd) = civil_from_days(days);
    let wd = weekday(days);
    let mut db = [0u8; 4];
    let n = push_dec(&mut db, dd);
    draw_text_px(x + 10, y + 8, &db[..n], 28.0, COLOR_TEXT_MAIN);
    let mut mb = [0u8; 12];
    let mut m = 0;
    if unsafe { LANG_ZH } {
        m += push_dec(&mut mb[m..], mo);
        mb[m..m + 3].copy_from_slice(b"\xE6\x9C\x88");
        m += 3;
    } else {
        mb[m..m + 3].copy_from_slice(MONTH_EN[(mo - 1) as usize].as_bytes());
        m += 3;
    }
    draw_text_px(x + 52, y + 10, &mb[..m], 13.0, COLOR_TEXT_DIM);
    draw_text_px(x + 52, y + 26, weekday_label(wd), 13.0, COLOR_TEXT_DIM);
    let mut ev = [0u8; 24];
    let en = unsafe { read_file(NEXT_EVENT_PATH, &mut ev) };
    let ev_s: &[u8] = if en > 0 {
        &ev[..en as usize]
    } else {
        txt("No events today", "今日暂无安排")
    };
    draw_label_trunc(x + 10, y + h - 20, ev_s, 11.0, w - 20, COLOR_TEXT_DIM);
}

/// 相册小组件（2x2）：轮播最近照片（每 3s 切换，简笔画 + 渐变底色）。
fn draw_widget_gallery(x: i32, y: i32, w: i32, h: i32) {
    let t = unsafe { GALLERY_TICK % 3 };
    let (top, bottom) = match t {
        0 => (0x00_2B_4B_7A, 0x00_0F_1B_33),
        1 => (0x00_4A_2B_6A, 0x00_1E_10_33),
        _ => (0x00_2B_6A_4A, 0x00_10_2B_1E),
    };
    let r = widget_radius(2, 2);
    fill_vgrad_masked(x, y, w, h, r, top, bottom);
    let white = 0x00_E8_EE_F8;
    match t {
        0 => {
            draw_circle(x + w / 2, y + h / 2 - 8, 9, white);
            draw_line(x + 6, y + h - 18, x + w / 2 - 4, y + h / 2 + 10, white);
            draw_line(x + w / 2 - 4, y + h / 2 + 10, x + w - 8, y + h - 18, white);
            draw_line(x + 12, y + h - 12, x + w - 12, y + h - 12, white);
        }
        1 => {
            draw_circle(x + w / 2, y + h / 2 - 6, 6, 0x00_FF_D0_4D);
            for i in 0..6 {
                put_pixel(x + 14 + (i % 3) * 18, y + 14 + (i / 3) * 16, white);
                put_pixel(x + 16 + (i % 3) * 18, y + 16 + (i / 3) * 16, white);
            }
            draw_line(x + 6, y + h - 16, x + w - 6, y + h - 16, white);
        }
        _ => {
            draw_line(x + 10, y + h - 22, x + w - 10, y + h - 22, 0x00_7F_C8_9F);
            draw_line(x + 8, y + h - 14, x + w - 8, y + h - 14, 0x00_7F_C8_9F);
            draw_line(x + 6, y + h - 8, x + w - 6, y + h - 8, 0x00_7F_C8_9F);
        }
    }
    draw_text_px(
        x + 10,
        y + h - 22,
        txt("Recent photos", "最近照片"),
        12.0,
        COLOR_TEXT,
    );
    for i in 0..3 {
        let c = if i as usize == t {
            COLOR_TEXT
        } else {
            0x00_78_85_98
        };
        fill_rect(x + w / 2 - 16 + i as i32 * 13, y + h - 8, 7, 7, c);
    }
}

/// 电池小组件（1x1）：电量 + 充电状态。
fn draw_widget_battery(x: i32, y: i32, w: i32, _h: i32) {
    let level = read_number_cached(1, BATTERY_PATH, 85).min(100);
    let charging = read_number_cached(2, CHARGING_PATH, 0) != 0;
    let bx = x + (w - 30) / 2;
    let by = y + 6;
    // 电池轮廓：浅色玻璃上用深色描边。
    draw_rect(bx, by, 30, 16, COLOR_TEXT_MAIN);
    fill_rect(bx + 30, by + 5, 3, 6, COLOR_TEXT_MAIN);
    let fw = (26 * level / 100).max(2) as i32;
    let fc = if level > 20 { COLOR_ACCENT } else { COLOR_WARN };
    fill_round_rect(bx + 2, by + 2, fw, 12, 3, fc);
    if charging {
        let c = 0x00_FF_D0_4D;
        draw_line(bx + 17, by - 6, bx + 12, by + 8, c);
        draw_line(bx + 12, by + 8, bx + 16, by + 8, c);
        draw_line(bx + 16, by + 8, bx + 13, by + 22, c);
    }
    let mut pb = [0u8; 5];
    let n = push_dec(&mut pb, level);
    pb[n] = b'%';
    let pw = text_width_px(&pb[..n + 1], 12.0);
    draw_text_px(x + (w - pw) / 2, y + 26, &pb[..n + 1], 12.0, COLOR_TEXT_MAIN);
}

fn draw_widget(kind: WidgetKind, x: i32, y: i32, w: i32, h: i32, wc: i32, hc: i32) {
    let r = widget_radius(wc, hc);
    if unsafe { EDIT_MODE } {
        // 编辑模式：浅色实心玻璃 + 浅灰描边（避免每帧模糊开销）。
        draw_glass_solid(x, y, w, h, r, COLOR_PANEL_ROW, 220, COLOR_BORDER);
    } else {
        draw_glass(x, y, w, h, r);
    }
    match kind {
        WidgetKind::Clock => draw_widget_clock(x, y, w, h),
        WidgetKind::Weather => draw_widget_weather(x, y, w, h),
        WidgetKind::Calendar => draw_widget_calendar(x, y, w, h),
        WidgetKind::Gallery => draw_widget_gallery(x, y, w, h),
        WidgetKind::Battery => draw_widget_battery(x, y, w, h),
    }
}

/// 小组件点击 → 启动对应应用。
fn widget_launch(kind: WidgetKind) {
    let key = match kind {
        WidgetKind::Clock => "clock",
        WidgetKind::Weather => "weather",
        WidgetKind::Calendar => "calendar",
        WidgetKind::Gallery => "gallery",
        WidgetKind::Battery => "settings",
    };
    if let Some(idx) = app_index_by_key(key) {
        start_launch(idx);
    }
}

// ==================== 文件夹 ====================

fn folder_panel_rect() -> (i32, i32, i32, i32) {
    (
        (DISPLAY_W as i32 - FOLDER_PANEL_W as i32) / 2,
        (NAV_Y - FOLDER_PANEL_H as i32) / 2,
        FOLDER_PANEL_W as i32,
        FOLDER_PANEL_H as i32,
    )
}

fn folder_icon_center() -> (i32, i32) {
    unsafe {
        let cols = GRID_COLS;
        for page in 0..PAGE_COUNT {
            let mut placed = [PlacedItem::zero(); MAX_PAGE_ITEMS];
            let n = layout_page(&PAGES[page], cols, &mut placed);
            for i in 0..n {
                if let ItemKind::Folder = placed[i].kind {
                    let (x, y, w, h) = item_rect(&placed[i], cols);
                    return (x + w / 2, y + h / 2);
                }
            }
        }
    }
    (DISPLAY_W as i32 / 2, DISPLAY_H as i32 / 2)
}

/// 全屏压暗（blend(c, 0x00050A14, 150)），文件夹打开时使用。
fn dim_screen() {
    unsafe {
        if !DIM_LUT_READY {
            for v in 0..256u32 {
                DIM_LUT[v as usize] = ((v * 105 + 5 * 150) / 255) << 16
                    | ((v * 105 + 10 * 150) / 255) << 8
                    | ((v * 105 + 20 * 150) / 255);
            }
            DIM_LUT_READY = true;
        }
        for i in 0..DISPLAY_W * DISPLAY_H {
            let c = BACK_BUF[i];
            BACK_BUF[i] = (DIM_LUT[((c >> 16) & 0xFF) as usize] << 16)
                | (DIM_LUT[((c >> 8) & 0xFF) as usize] << 8)
                | DIM_LUT[(c & 0xFF) as usize];
        }
    }
}

/// 构建文件夹面板瓦片：捕获（已压暗的）桌面 → 模糊玻璃 → 内容（名称/3x3 图标）。
fn folder_build_tile() {
    unsafe {
        let (px, py, pw, ph) = folder_panel_rect();
        FOLDER_TILE_X = px;
        FOLDER_TILE_Y = py;
        // 1. 原图（已压暗的桌面）→ FOLDER_ORIG
        for dy in 0..ph {
            for dx in 0..pw {
                let sx = px + dx;
                let sy = py + dy;
                let c = if sx >= 0 && sy >= 0 && sx < DISPLAY_W as i32 && sy < DISPLAY_H as i32 {
                    BACK_BUF[sy as usize * DISPLAY_W + sx as usize]
                } else {
                    0
                };
                FOLDER_ORIG[dy as usize * FOLDER_PANEL_W + dx as usize] = c;
            }
        }
        // 2-3. 液态玻璃面板：先复制原图（圆角外保持原样），再以统一玻璃样式
        //      覆盖圆角内部（模糊源 = BACK_BUF 中已压暗的桌面）。
        FOLDER_TILE.copy_from_slice(&FOLDER_ORIG);
        FOLDER_TILE_ACTIVE = true;
        draw_glass(px, py, pw, ph, FOLDER_RADIUS);
        FOLDER_TILE_ACTIVE = false;
        // 4. 内容（名称 + 3x3 图标 + 提示）
        FOLDER_TILE_ACTIVE = true;
        let name = folder_name_bytes();
        let nw = text_width_px(name, FOLDER_NAME_PX);
        draw_text_px(
            px + (pw - nw) / 2,
            py + 26,
            name,
            FOLDER_NAME_PX,
            COLOR_TEXT_MAIN,
        );
        let hint = txt("Tap name to rename", "点击名称重命名");
        let hw = text_width_px(hint, 11.0);
        draw_text_px(px + (pw - hw) / 2, py + 52, hint, 11.0, COLOR_TEXT_DIM);
        let cell = FOLDER_ICON + 16;
        let ox = px + (pw - (3 * cell - 16)) / 2;
        let oy = py + 88;
        for i in 0..FOLDER_COUNT {
            let col = (i % 3) as i32;
            let row = (i / 3) as i32;
            let ix = ox + col * cell;
            let iy = oy + row * cell;
            let app = &APPS[FOLDER_APPS[i]];
            draw_app_icon(app, ix, iy, FOLDER_ICON);
            let name = app.display_name().as_bytes();
            let w = text_width_px(name, 11.0);
            let maxw = (cell - 4).min(CELL_W);
            if w > maxw {
                draw_label_trunc(
                    ix + (FOLDER_ICON - maxw) / 2,
                    iy + FOLDER_ICON + 2,
                    name,
                    11.0,
                    maxw,
                    COLOR_TEXT_DIM,
                );
            } else {
                draw_text_px(
                    ix + (FOLDER_ICON - w) / 2,
                    iy + FOLDER_ICON + 2,
                    name,
                    11.0,
                    COLOR_TEXT_DIM,
                );
            }
        }
        if FOLDER_COUNT == 0 {
            let empty = txt("Folder is empty", "文件夹为空");
            let ew = text_width_px(empty, 13.0);
            draw_text_px(px + (pw - ew) / 2, oy + 100, empty, 13.0, COLOR_TEXT_DIM);
        }
        let foot = txt("Tap outside to close", "点击外部关闭");
        let fw = text_width_px(foot, 12.0);
        draw_text_px(px + (pw - fw) / 2, py + ph - 38, foot, 12.0, COLOR_TEXT_DIM);
        FOLDER_TILE_ACTIVE = false;
        FOLDER_TILE_DIRTY = false;
    }
}

/// 把面板瓦片以 `s` 缩放、锚点 `(ax, ay)` 绘制（350ms 弹簧打开动画）。
fn folder_blit(s: f32, ax: i32, ay: i32) {
    if s <= 0.02 {
        return;
    }
    let pw = FOLDER_PANEL_W as i32;
    let ph = FOLDER_PANEL_H as i32;
    let half_w = (pw as f32 * s * 0.5) as i32;
    let half_h = (ph as f32 * s * 0.5) as i32;
    let x0 = ax - half_w;
    let y0 = ay - half_h;
    let x1 = ax + half_w;
    let y1 = ay + half_h;
    let step = if s > 0.9 { 1 } else { 2 };
    let mut oy = y0;
    while oy <= y1 {
        let mut ox = x0;
        while ox <= x1 {
            let lx = ((ox - ax) as f32 / s + pw as f32 * 0.5) as usize;
            let ly = ((oy - ay) as f32 / s + ph as f32 * 0.5) as usize;
            if lx < FOLDER_PANEL_W && ly < FOLDER_PANEL_H {
                let c = unsafe { FOLDER_TILE[ly * FOLDER_PANEL_W + lx] };
                put_pixel(ox, oy, c);
                if step == 2 {
                    if ox + 1 <= x1 {
                        put_pixel(ox + 1, oy, c);
                    }
                    if oy + 1 <= y1 {
                        put_pixel(ox, oy + 1, c);
                        if ox + 1 <= x1 {
                            put_pixel(ox + 1, oy + 1, c);
                        }
                    }
                }
            }
            ox += step;
        }
        oy += step;
    }
}

fn folder_start_open() {
    unsafe {
        if FOLDER_PHASE != FolderPhase::Closed || MOTION.phase != Phase::Idle {
            return;
        }
        FOLDER_PHASE = FolderPhase::Opening;
        FOLDER_ANIM = Spring::folder();
        FOLDER_ANIM.pos = 0.1;
        FOLDER_ANIM.target = 1.0;
        FOLDER_ANIM.vel = 0.0;
        FOLDER_LAST_T = now_us();
        let (ax, ay) = folder_icon_center();
        FOLDER_ANCHOR_X = ax;
        FOLDER_ANCHOR_Y = ay;
        FOLDER_TILE_DIRTY = true;
        NEEDS_REDRAW = true;
        print("[launcher] 已打开文件夹\n");
    }
}

fn folder_start_close() {
    unsafe {
        if FOLDER_PHASE != FolderPhase::Open && FOLDER_PHASE != FolderPhase::Opening {
            return;
        }
        FOLDER_PHASE = FolderPhase::Closing;
        FOLDER_ANIM = Spring::folder();
        FOLDER_ANIM.pos = 1.0;
        FOLDER_ANIM.target = 0.1;
        FOLDER_ANIM.vel = 0.0;
        FOLDER_LAST_T = now_us();
        NEEDS_REDRAW = true;
    }
}

/// 文件夹内容命中测试（3x3 图标网格）。
fn folder_hit(x: i32, y: i32) -> Option<usize> {
    unsafe {
        let (px, py, pw, _) = folder_panel_rect();
        let cell = FOLDER_ICON + 16;
        let ox = px + (pw - (3 * cell - 16)) / 2;
        let oy = py + 88;
        for i in 0..FOLDER_COUNT {
            let col = (i % 3) as i32;
            let row = (i / 3) as i32;
            let ix = ox + col * cell;
            let iy = oy + row * cell;
            if x >= ix && x < ix + FOLDER_ICON && y >= iy && y < iy + FOLDER_ICON {
                return Some(FOLDER_APPS[i]);
            }
        }
    }
    None
}

// ==================== 文件夹重命名键盘 ====================

const KB_Y0: i32 = 704;
const KB: [[u8; 10]; 4] = [
    *b"QWERTYUIOP",
    [b'A', b'S', b'D', b'F', b'G', b'H', b'J', b'K', b'L', 0],
    [b'Z', b'X', b'C', b'V', b'B', b'N', b'M', b' ', 1, 0],
    *b"1234567890",
];

fn rename_open() {
    unsafe {
        RENAME_BUF.copy_from_slice(&FOLDER_NAME);
        RENAME_LEN = FOLDER_NAME_LEN;
        RENAME_OPEN = true;
        NEEDS_REDRAW = true;
    }
}

fn rename_key_hit(x: i32, y: i32) {
    unsafe {
        let row = ((y - KB_Y0) / 48) as usize;
        let col = (x / 54) as usize;
        if row >= 4 || col >= 10 {
            return;
        }
        let v = KB[row][col];
        match v {
            0 => {
                if RENAME_LEN > 0 {
                    RENAME_LEN -= 1;
                }
            }
            1 => {
                FOLDER_NAME[..RENAME_LEN].copy_from_slice(&RENAME_BUF[..RENAME_LEN]);
                FOLDER_NAME_LEN = RENAME_LEN;
                if !write_file(FOLDER_NAME_PATH, &FOLDER_NAME[..FOLDER_NAME_LEN]) {
                    show_toast(txt("Failed to save name", "保存失败"));
                }
                RENAME_OPEN = false;
                FOLDER_TILE_DIRTY = true;
            }
            _ => {
                if RENAME_LEN < RENAME_BUF.len() {
                    RENAME_BUF[RENAME_LEN] = v;
                    RENAME_LEN += 1;
                }
            }
        }
        NEEDS_REDRAW = true;
    }
}

fn draw_rename_panel() {
    // 液态玻璃重命名面板（模糊桌面 + 浅色着色，深字浅玻璃按键）。
    draw_glass(0, 640, DISPLAY_W as i32, 264, 0);
    let title = txt("Rename folder", "重命名文件夹");
    draw_text_px(16, 616, title, 13.0, COLOR_TEXT_MAIN);
    let name = unsafe { &RENAME_BUF[..RENAME_LEN] };
    let w = text_width_px(name, 20.0);
    let nx = (DISPLAY_W as i32 - w) / 2;
    draw_text_px(nx, 650, name, 20.0, COLOR_TEXT_MAIN);
    draw_line(nx + w + 2, 654, nx + w + 2, 672, COLOR_ACCENT);
    for row in 0..4 {
        for col in 0..10 {
            let kx = col * 54;
            let ky = KB_Y0 + row * 48;
            // 液态玻璃按键。
            draw_glass_key(kx + 1, ky + 1, 52, 46);
            let v = KB[row as usize][col as usize];
            let label: &[u8] = match v {
                0 => txt("DEL", "删"),
                1 => txt("OK", "确定"),
                b' ' => txt("SPACE", "空格"),
                _ => core::slice::from_ref(&v),
            };
            let lw = text_width_px(label, 14.0);
            draw_text_px(kx + (52 - lw) / 2, ky + 15, label, 14.0, COLOR_TEXT_MAIN);
        }
    }
}

// ==================== 编辑模式拖拽 ====================

fn on_press(x: i32, y: i32) {
    unsafe {
        if EDIT_MODE && FOLDER_PHASE == FolderPhase::Closed && !RENAME_OPEN {
            if let Some(p) = hit_item(x, y) {
                if let ItemKind::App(app) = p.kind {
                    DRAG_ACTIVE = true;
                    DRAG_ITEM = p.index;
                    DRAG_APP = app;
                    DRAG_MOVED = false;
                    DRAG_X = x;
                    DRAG_Y = y;
                    NEEDS_REDRAW = true;
                }
            }
        } else if FOLDER_PHASE == FolderPhase::Closed && !RENAME_OPEN {
            if let Some(app) = hit_launchable(x, y) {
                start_press(app);
            }
        }
    }
}

/// 文件夹弹簧动画推进（打开/关闭，350ms）；返回是否仍在动画中。
fn folder_advance(now: u64) -> bool {
    unsafe {
        match FOLDER_PHASE {
            FolderPhase::Opening | FolderPhase::Closing => {
                let dt_ms = now.saturating_sub(FOLDER_LAST_T).max(1) as f32 / 1000.0;
                FOLDER_LAST_T = now;
                if FOLDER_ANIM.step_ms(dt_ms) {
                    FOLDER_ANIM.pos = FOLDER_ANIM.target;
                    if FOLDER_PHASE == FolderPhase::Opening {
                        FOLDER_PHASE = FolderPhase::Open;
                    } else {
                        FOLDER_PHASE = FolderPhase::Closed;
                        if FOLDER_LAUNCH != usize::MAX {
                            let app = FOLDER_LAUNCH;
                            FOLDER_LAUNCH = usize::MAX;
                            start_launch(app);
                        }
                        NEEDS_REDRAW = true;
                    }
                }
                true
            }
            _ => false,
        }
    }
}

/// 编辑模式松手：移动则换位 / 移入文件夹，未移动则视为点击移除。
fn edit_drop(x: i32, y: i32) {
    unsafe {
        if DRAG_ITEM == usize::MAX {
            return;
        }
        let page = CURRENT_PAGE;
        let drag_app = DRAG_APP;
        let drag_item = DRAG_ITEM;
        if !DRAG_MOVED {
            DRAG_ACTIVE = false;
            DRAG_ITEM = usize::MAX;
            DRAG_APP = usize::MAX;
            if drag_app != usize::MAX {
                remove_app(drag_app);
            }
            return;
        }
        layout_current();
        let cols = GRID_COLS;
        let mut target: Option<(usize, ItemKind)> = None;
        for i in 0..PLACED_COUNT {
            let p = PLACED[i];
            if p.index == drag_item {
                continue;
            }
            let (ix, iy, iw, ih) = item_rect(&p, cols);
            if x >= ix && x < ix + iw && y >= iy && y < iy + ih {
                target = Some((p.index, p.kind));
                break;
            }
        }
        match target {
            Some((t_idx, ItemKind::App(_))) => {
                let a = core::ptr::addr_of_mut!(PAGES[page].items[drag_item]);
                let b = core::ptr::addr_of_mut!(PAGES[page].items[t_idx]);
                let tmp = core::ptr::read(a);
                core::ptr::write(a, core::ptr::read(b));
                core::ptr::write(b, tmp);
                show_toast(txt("App moved", "应用已移动"));
            }
            Some((_, ItemKind::Folder)) => {
                let mut dup = false;
                for i in 0..FOLDER_COUNT {
                    if FOLDER_APPS[i] == drag_app {
                        dup = true;
                    }
                }
                if dup {
                    show_toast(txt("Already in folder", "已在文件夹"));
                } else if FOLDER_COUNT < FOLDER_MAX && drag_app != usize::MAX {
                    FOLDER_APPS[FOLDER_COUNT] = drag_app;
                    FOLDER_COUNT += 1;
                    for i in 0..PAGES[page].count {
                        if PAGES[page].items[i].kind == ItemKind::App(drag_app) {
                            let a = core::ptr::addr_of_mut!(PAGES[page].items[i]);
                            core::ptr::write(
                                a,
                                core::ptr::read(core::ptr::addr_of!(
                                    PAGES[page].items[PAGES[page].count - 1]
                                )),
                            );
                            PAGES[page].count -= 1;
                            break;
                        }
                    }
                    show_toast(txt("Moved to folder", "已移入文件夹"));
                    FOLDER_TILE_DIRTY = true;
                } else {
                    show_toast(txt("Folder is full", "文件夹已满"));
                }
            }
            _ => {
                show_toast(txt("Snapped back", "已回到原位"));
            }
        }
        DRAG_ACTIVE = false;
        DRAG_MOVED = false;
        DRAG_ITEM = usize::MAX;
        DRAG_APP = usize::MAX;
        NEEDS_REDRAW = true;
    }
}

/// 鼠标长按 → 进入编辑模式（与触摸长按一致）。
fn mouse_long_press(x: i32, y: i32) {
    unsafe {
        if EDIT_MODE || FOLDER_PHASE != FolderPhase::Closed || RENAME_OPEN {
            return;
        }
        if let Some(p) = hit_item(x, y) {
            if let ItemKind::App(_) = p.kind {
                EDIT_MODE = true;
                show_toast(txt("Edit mode", "编辑模式"));
                NEEDS_REDRAW = true;
            }
        }
    }
}

// ==================== 锁屏渲染 ====================

/// 以 `scale` 倍放大绘制单个 8x8 字形（锁屏大时钟用）。
fn draw_big_glyph(x: i32, y: i32, c: u8, scale: i32, color: u32) {
    let g = glyph(c);
    for (row, bits) in g.iter().enumerate() {
        for col in 0..8 {
            if bits & (0x80 >> col) != 0 {
                fill_rect(
                    x + col as i32 * scale,
                    y + row as i32 * scale,
                    scale,
                    scale,
                    color,
                );
            }
        }
    }
}

/// 居中绘制一行 8x8 字形文本（scale 放大）。
fn draw_big_text(cx: i32, y: i32, s: &[u8], scale: i32, color: u32) {
    let total_w = s.len() as i32 * 8 * scale;
    let mut x = cx - total_w / 2;
    for &c in s {
        draw_big_glyph(x, y, c, scale, color);
        x += 8 * scale;
    }
}

/// 左对齐绘制一行 8x8 字形文本（scale 放大）。
fn draw_big_glyphs(mut x: i32, y: i32, s: &[u8], scale: i32, color: u32) {
    for &c in s {
        draw_big_glyph(x, y, c, scale, color);
        x += 8 * scale;
    }
}

/// 天数（自 1970-01-01）→ (年, 月, 日)（Howard Hinnant civil_from_days）。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 把 u32 写为十进制数字（不补零），返回字节数。
fn push_dec(buf: &mut [u8], mut v: u32) -> usize {
    let mut tmp = [0u8; 12];
    let mut i = tmp.len();
    if v == 0 {
        tmp[i - 1] = b'0';
        i -= 1;
    } else {
        while v > 0 {
            i -= 1;
            tmp[i] = b'0' + (v % 10) as u8;
            v /= 10;
        }
    }
    let n = tmp.len() - i;
    // 容量感知：放不下时截断到 buf.len()（此前 buf[..n] 直接 panic）。
    let k = n.min(buf.len());
    buf[..k].copy_from_slice(&tmp[tmp.len() - k..]);
    k
}

/// 锁屏界面布局常量。
const LOCK_KEY_W: i32 = 76;
const LOCK_KEY_H: i32 = 58;
const LOCK_KEY_GAP: i32 = 14;
const LOCK_PAD_X: i32 = (DISPLAY_W as i32 - (3 * LOCK_KEY_W + 2 * LOCK_KEY_GAP)) / 2;
const LOCK_PAD_Y: i32 = 560;
/// 图案 3x3 点阵布局。
const LOCK_PAT_R: i32 = 20;
const LOCK_PAT_GAP: i32 = 84;
const LOCK_PAT_X0: i32 = (DISPLAY_W as i32 - 2 * LOCK_PAT_GAP) / 2;
const LOCK_PAT_Y0: i32 = 430;

/// 图案点 (i:0..=8) 的屏幕坐标。
fn lock_pat_center(i: usize) -> (i32, i32) {
    let row = (i / 3) as i32;
    let col = (i % 3) as i32;
    (
        LOCK_PAT_X0 + col * LOCK_PAT_GAP,
        LOCK_PAT_Y0 + row * LOCK_PAT_GAP,
    )
}

/// 渲染锁屏：深色壁纸 + 大时钟 + 日期 + 解锁区（静态锁定态）。
fn render_lock_screen() {
    draw_lock_background(0);
    draw_lock_content(0);
    // 状态栏时间（保持可见）。
    draw_status_bar();
}

/// 锁屏背景：垂直渐变整层绘制，整体垂直偏移 `off`（≤ 0，0 = 覆盖全屏）。
/// 深色主题 = 深蓝（顶层 #101527 → 底层 #1B2436）；浅色主题 = 浅灰蓝
/// （#F7FAFD → #E7EFF7），与 OOBE/桌面保持一致。
/// 不用 draw_background + 双重压暗（每帧 ~150 万次混合/查表），
/// 渐变按行预计算后整行直写，锁屏首帧显著更快。
/// 转场时仅绘制层覆盖的屏幕行（层上移出屏的部分露出下方桌面）。
fn draw_lock_background(off: i32) {
    unsafe {
        let (top_r, top_g, top_b, bot_r, bot_g, bot_b) = if THEME_DARK {
            (0x10u32, 0x15u32, 0x27u32, 0x1Bu32, 0x24u32, 0x36u32)
        } else {
            (0xF7u32, 0xFAu32, 0xFDu32, 0xE7u32, 0xEFu32, 0xF7u32)
        };
        // 层上移 |off| 时：屏幕可见行 0..(H+off) 对应渐变行 (-off)..H。
        let end = (DISPLAY_H as i32 + off).min(DISPLAY_H as i32);
        // off > 0（跟手上滑后往回下拖）：层整体下移，顶部 y<off 的行不再属于
        // 锁屏层（露出下方桌面）——跳过这些行；否则 gy<0 经 as u32 回绕成
        // ~4×10⁹，乘 255 再回绕，渐变顶行变成垃圾色。
        let start = if off > 0 { off } else { 0 };
        let mut y = start;
        while y < end {
            let gy = y - off;
            let t = (gy as u32 * 255 / DISPLAY_H as u32).min(255);
            let inv = 255 - t;
            let r = (top_r * inv + bot_r * t) / 255;
            let g = (top_g * inv + bot_g * t) / 255;
            let b = (top_b * inv + bot_b * t) / 255;
            let row = (r << 16) | (g << 8) | b;
            let iy = y as usize * DISPLAY_W;
            for x in 0..DISPLAY_W {
                BACK_BUF[iy + x] = row;
            }
            y += 1;
        }
    }
}

/// 锁屏内容层：时钟/日期/锁图标/提示/指示点/键盘或图案，整体偏移 `off`。
/// 供静态锁屏（off = 0）与解锁/锁定转场（off 随动画推进）复用。
fn draw_lock_content(off: i32) {
    unsafe {
        // 时钟/日期/提示文字与指示点颜色随主题（浅色 = 深字灰底）。
        let clock_col = if THEME_DARK {
            COLOR_TEXT
        } else {
            COLOR_TEXT_MAIN
        };
        let sub_col = if THEME_DARK {
            0x00_B8_C2_D2
        } else {
            COLOR_TEXT_DIM
        };
        let dot_empty = if THEME_DARK {
            0x00_45_55_6E
        } else {
            0x00_C7_CC_D3
        };
        // 按键上的文字/图案：深色主题白字，浅色主题深字（浅玻璃上看不见白字）。
        let key_col = if THEME_DARK {
            COLOR_TEXT
        } else {
            COLOR_TEXT_MAIN
        };
        // 大时钟 HH:MM（8x8 字形 5 倍放大）。
        let mut hhmm = [0u8; 5];
        time_hhmm(&mut hhmm);
        draw_big_text(DISPLAY_W as i32 / 2, 150 + off, &hhmm, 5, clock_col);
        // 日期行（YYYY-MM-DD，8x8 字形 1 倍）。
        let us = now_us();
        let secs = (us / 1_000_000) as i64;
        let (y, mo, d) = civil_from_days(secs / 86400);
        let mut date = [0u8; 11];
        let mut n = push_dec(&mut date, y as u32);
        date[n] = b'-';
        n += 1;
        n += push_dec(&mut date[n..], mo);
        date[n] = b'-';
        n += 1;
        n += push_dec(&mut date[n..], d);
        draw_big_text(DISPLAY_W as i32 / 2, 240 + off, &date[..n], 1, sub_col);
        // 锁图标（液态玻璃圆角方块 + 锁梁）。
        let (lx, ly) = (DISPLAY_W as i32 / 2 - 10, 300 + off);
        draw_glass(lx, ly, 20, 16, 6);
        fill_rect(lx + 6, ly - 7, 8, 7, COLOR_ACCENT);
        fill_rect(lx, ly, 20, 16, COLOR_ACCENT);
        // 解锁提示 / 错误提示。
        if LOCK_FAIL_TICKS > 0 {
            draw_big_text(
                DISPLAY_W as i32 / 2,
                340 + off,
                txt("WRONG PASSCODE", "密码错误"),
                1,
                COLOR_WARN,
            );
        } else {
            match LOCK_SEC_TYPE {
                0 => draw_big_text(
                    DISPLAY_W as i32 / 2,
                    340 + off,
                    txt("Swipe up to unlock", "上滑解锁"),
                    1,
                    sub_col,
                ),
                1 => draw_big_text(
                    DISPLAY_W as i32 / 2,
                    340 + off,
                    txt("Enter PIN", "输入 PIN"),
                    1,
                    sub_col,
                ),
                2 => draw_big_text(
                    DISPLAY_W as i32 / 2,
                    340 + off,
                    txt("Draw pattern", "绘制图案"),
                    1,
                    sub_col,
                ),
                _ => draw_big_text(
                    DISPLAY_W as i32 / 2,
                    340 + off,
                    txt("Enter password", "输入密码"),
                    1,
                    sub_col,
                ),
            }
        }
        // 已输入长度指示点。
        let len = lock_input_display_len();
        let dots = match LOCK_SEC_TYPE {
            1 => 4,
            3 => 8,
            _ => 9,
        };
        let gap = 18;
        let total = dots as i32 * gap;
        let x0 = DISPLAY_W as i32 / 2 - total / 2 + gap / 2;
        for d in 0..dots {
            let filled = (d as usize) < len;
            let c = if filled { COLOR_ACCENT } else { dot_empty };
            fill_rect(x0 + d as i32 * gap - 4, 372 + off, 8, 8, c);
        }
        // 输入区：PIN/密码数字键盘 / 图案点阵。
        match LOCK_SEC_TYPE {
            0 => {
                // 滑动解锁：液态玻璃提示条（上滑手势由 handle_lock_input 处理）。
                draw_glass(DISPLAY_W as i32 / 2 - 60, 754 + off, 120, 16, 8);
            }
            1 | 3 => {
                // 3x4 键盘：1-9 * 0 #（* 为删除，# 为确认）—— 液态玻璃按键。
                let keys: [u8; 12] = [
                    b'1', b'2', b'3', b'4', b'5', b'6', b'7', b'8', b'9', b'*', b'0', b'#',
                ];
                for i in 0..12 {
                    let row = (i / 3) as i32;
                    let col = (i % 3) as i32;
                    let x = LOCK_PAD_X + col * (LOCK_KEY_W + LOCK_KEY_GAP);
                    let y = LOCK_PAD_Y + off + row * (LOCK_KEY_H + LOCK_KEY_GAP);
                    draw_glass_key(x, y, LOCK_KEY_W, LOCK_KEY_H);
                    let ch = keys[i];
                    let label: &[u8] = match ch {
                        b'*' => txt("DEL", "删"),
                        b'#' => txt("OK", "确定"),
                        _ => &[ch],
                    };
                    let lw = label.len() as i32 * 8;
                    draw_big_text(
                        x + LOCK_KEY_W / 2,
                        y + (LOCK_KEY_H - 8) / 2,
                        label,
                        1,
                        key_col,
                    );
                    let _ = lw;
                }
            }
            _ => {
                // 3x3 图案点阵。
                for i in 0..9 {
                    let (cx, cy0) = lock_pat_center(i);
                    let cy = cy0 + off;
                    let selected = LOCK_PATTERN[..LOCK_PATTERN_LEN].contains(&(i as u8 + 1));
                    let c = if selected { COLOR_ACCENT } else { dot_empty };
                    fill_rect(cx - 3, cy - 3, 6, 6, c);
                    draw_circle(cx, cy, LOCK_PAT_R, c);
                }
                // 已选点连线。
                for k in 1..LOCK_PATTERN_LEN {
                    let a = lock_pat_center((LOCK_PATTERN[k - 1] - 1) as usize);
                    let b = lock_pat_center((LOCK_PATTERN[k] - 1) as usize);
                    draw_line(a.0, a.1 + off, b.0, b.1 + off, COLOR_ACCENT);
                }
                // 下方删除键 —— 液态玻璃。
                draw_glass(DISPLAY_W as i32 / 2 - 40, 720 + off, 80, 44, 12);
                draw_big_text(
                    DISPLAY_W as i32 / 2,
                    736 + off,
                    txt("CLEAR", "清除"),
                    1,
                    key_col,
                );
            }
        }
        // 充电动效（ColorOS 幻光影流动风）：呼吸光晕药丸 + 双圈涟漪。
        draw_lock_charging(off);
    }
}

// ==================== 锁屏充电动效 ====================

/// 充电状态缓存（每 1s 重读 /data/oobe/charging，避免主循环逐帧读文件）。
static mut CHG_CACHED: bool = false;
static mut CHG_CHECK_T0: u64 = 0;

/// 锁屏充电呼吸/涟漪是否活动（主循环唤醒条件；转场期间由 LOCK_PHASE 抑制）。
fn lock_charging_anim() -> bool {
    unsafe {
        if !LOCKED || LOCK_PHASE != LockPhase::None {
            return false;
        }
        let now = now_us();
        if now.saturating_sub(CHG_CHECK_T0) > 1_000_000 {
            CHG_CHECK_T0 = now;
            CHG_CACHED = read_number(CHARGING_PATH, 0) != 0;
        }
        CHG_CACHED
    }
}

/// 在 BACK_BUF 上以 alpha 混合画 1px 圆环（整数中点圆，无 sin/cos）。
unsafe fn blend_ring(cx: i32, cy: i32, r: i32, color: u32, alpha: u32) {
    if r <= 0 || alpha == 0 {
        return;
    }
    let w = DISPLAY_W as i32;
    let h = DISPLAY_H as i32;
    let a = alpha.min(255);
    let mut px: i32 = 0;
    let mut py: i32 = r;
    let mut d: i32 = 1 - r;
    while px <= py {
        let pts = [
            (cx + px, cy + py),
            (cx - px, cy + py),
            (cx + px, cy - py),
            (cx - px, cy - py),
            (cx + py, cy + px),
            (cx - py, cy + px),
            (cx + py, cy - px),
            (cx - py, cy - px),
        ];
        for (x, y) in pts {
            if x >= 0 && x < w && y >= 0 && y < h {
                let idx = y as usize * DISPLAY_W + x as usize;
                BACK_BUF[idx] = blend(BACK_BUF[idx], color, a);
            }
        }
        if d < 0 {
            d += 2 * px + 3;
        } else {
            d += 2 * (px - py) + 5;
            py -= 1;
        }
        px += 1;
    }
}

/// 锁屏充电指示：电池药丸 + 呼吸光晕（三角波 1.2s）+ 双圈错峰涟漪
/// （周期 1.6s、圈间滞后 0.8s，半径 18→64、alpha 120→0），
/// 位于 PIN/图案输入区下方空档（cy=620），随锁屏层整体平移。
unsafe fn draw_lock_charging(off: i32) {
    if !CHG_CACHED {
        return;
    }
    let level = read_number_cached(3, BATTERY_PATH, 85).min(100);
    let cx = DISPLAY_W as i32 / 2;
    let cy = 620 + off;
    let now = now_us();
    for k in 0..2u64 {
        let t = ((now + 800_000 * k) % 1_600_000) as f32 / 1_600_000.0;
        let r = 18.0 + 46.0 * t;
        let a = (120.0 * (1.0 - t)) as u32;
        blend_ring(cx, cy, r as i32, COLOR_ACCENT, a);
    }
    let br = motion::triangle(now, 1_200_000);
    let glow = [70.0f32, 46.0, 24.0];
    for i in 0..3 {
        let a = (glow[i] * (0.55 + 0.45 * br)) as u32;
        blend_ring(cx, cy, 26 + i as i32 * 4, COLOR_ACCENT, a);
    }
    let pill_bg = if THEME_DARK {
        0x00_2A_36_48
    } else {
        0x00_EA_EF_F5
    };
    fill_round_rect(cx - 32, cy - 12, 64, 24, 12, pill_bg);
    let fw = (56 * level / 100) as i32;
    if fw > 0 {
        fill_round_rect(cx - 28, cy - 8, fw, 16, 8, COLOR_ACCENT);
    }
    let mut buf = [0u8; 5];
    let n = push_dec(&mut buf, level as u32);
    buf[n] = b'%';
    let col = if THEME_DARK {
        0x00_B8_C2_D2
    } else {
        COLOR_TEXT_DIM
    };
    draw_big_text(cx, cy - 34, &buf[..n + 1], 1, col);
}

/// 锁屏整层（背景 + 内容）垂直平移绘制，供解锁/锁定转场使用：
/// 调用前先绘制桌面内容，层滑出/滑入即露出/覆盖桌面。
fn draw_lock_layer(off: i32) {
    if off <= -(DISPLAY_H as i32) {
        return;
    }
    draw_lock_background(off);
    draw_lock_content(off);
    draw_status_bar();
}

/// 锁屏点击命中：数字键盘 / 图案点 / 清除键。返回 true 表示已消费。
fn lock_key_hit(x: i32, y: i32) -> bool {
    unsafe {
        match LOCK_SEC_TYPE {
            0 => {
                // 无安全：点提示条区域也解锁。
                if x >= DISPLAY_W as i32 / 2 - 60
                    && x < DISPLAY_W as i32 / 2 + 60
                    && y >= 750
                    && y < 780
                {
                    unlock_success();
                    return true;
                }
                false
            }
            1 | 3 => {
                for i in 0..12 {
                    let row = (i / 3) as i32;
                    let col = (i % 3) as i32;
                    let kx = LOCK_PAD_X + col * (LOCK_KEY_W + LOCK_KEY_GAP);
                    let ky = LOCK_PAD_Y + row * (LOCK_KEY_H + LOCK_KEY_GAP);
                    if x >= kx && x < kx + LOCK_KEY_W && y >= ky && y < ky + LOCK_KEY_H {
                        match i {
                            9 => {
                                // 删除
                                if LOCK_INPUT_LEN > 0 {
                                    LOCK_INPUT_LEN -= 1;
                                }
                            }
                            11 => lock_try_unlock(true),
                            _ => {
                                let ch = if i == 10 { b'0' } else { b'0' + i as u8 + 1 };
                                if LOCK_INPUT_LEN < LOCK_INPUT.len() {
                                    LOCK_INPUT[LOCK_INPUT_LEN] = ch;
                                    LOCK_INPUT_LEN += 1;
                                }
                                // 每次输入后校验：哈希匹配即解锁（支持任意长度
                                // PIN/密码）；输入满 16 位仍未匹配则判失败。
                                if LOCK_INPUT_LEN >= LOCK_INPUT.len() {
                                    lock_try_unlock(true);
                                } else {
                                    lock_try_unlock(false);
                                }
                            }
                        }
                        NEEDS_REDRAW = true;
                        return true;
                    }
                }
                false
            }
            _ => {
                // 图案点：命中 3x3 点。
                for i in 0..9 {
                    let (cx, cy) = lock_pat_center(i);
                    if (x - cx) * (x - cx) + (y - cy) * (y - cy)
                        <= (LOCK_PAT_R + 12) * (LOCK_PAT_R + 12)
                    {
                        let v = i as u8 + 1;
                        if LOCK_PATTERN_LEN == 0 || LOCK_PATTERN[LOCK_PATTERN_LEN - 1] != v {
                            if LOCK_PATTERN_LEN < LOCK_PATTERN.len() {
                                LOCK_PATTERN[LOCK_PATTERN_LEN] = v;
                                LOCK_PATTERN_LEN += 1;
                            }
                        }
                        NEEDS_REDRAW = true;
                        return true;
                    }
                }
                // 清除键
                if x >= DISPLAY_W as i32 / 2 - 40
                    && x < DISPLAY_W as i32 / 2 + 40
                    && y >= 720
                    && y < 764
                {
                    LOCK_PATTERN_LEN = 0;
                    NEEDS_REDRAW = true;
                    return true;
                }
                // 图案连满 4 点后自动校验（松开判定，见 handle_lock_input）。
                false
            }
        }
    }
}

/// 锁屏输入处理：点击解锁控件、上滑解锁（无安全模式）。
/// 返回 true 表示事件已被锁屏消费（不再进入桌面逻辑）。
fn handle_lock_input(g: Gesture) -> bool {
    unsafe {
        // 转场/回弹期间锁屏输入全部屏蔽（解锁上滑、PIN、图案一致）。
        if LOCK_PHASE != LockPhase::None {
            return true;
        }
        // 锁屏消费掉的手势不会走桌面路径：兜底取消可能残留的图标按压态
        // （否则 PressIn 弹簧永不收敛，每帧触发全屏重绘）。
        app_launch::cancel_press();
        match g {
            Gesture::Tap { x, y } => {
                // 状态栏区域（时间/图标）不响应，其余交给锁屏控件。
                if y >= STATUS_H {
                    lock_key_hit(x, y);
                }
                true
            }
            Gesture::Swipe { dy, dur_ms, .. } => {
                if LOCK_SEC_TYPE == 0 {
                    // 滑动解锁：跟手结算（越过阈值解锁，否则回弹）。
                    lock_swipe_settle(dy, dur_ms);
                } else if LOCK_SEC_TYPE == 2 && LOCK_PATTERN_LEN >= 4 {
                    lock_try_unlock(true);
                }
                true
            }
            _ => true,
        }
    }
}

fn render_frame() {
    unsafe {
        // 锁屏态：静态锁定整屏渲染锁屏；跟手/解锁/锁定转场期间先画桌面，
        // 再把锁屏整层按 LOCK_LAYER_OFF 平移（滑出露出桌面 / 滑入覆盖）。
        if LOCKED {
            if LOCK_PHASE == LockPhase::None && LOCK_LAYER_OFF == 0.0 {
                render_lock_screen();
            } else {
                draw_desktop_full();
                draw_lock_layer(LOCK_LAYER_OFF as i32);
            }
            return;
        }
        match MOTION.phase {
            // 转场期间：从快照缩放合成，避免重绘桌面内容
            Phase::Launch => {
                launch_blit(MOTION.cur_scale, MOTION.cx, MOTION.cy, MOTION.cur_fade);
            }
            Phase::Return => {
                zoom_blit(MOTION.cur_scale, MOTION.cx, MOTION.cy, MOTION.cur_fade);
            }
            // 恢复转场：应用末帧快照（FRAME_BUF）从图标原位放大到全屏。
            Phase::Resume => {
                zoom_blit(MOTION.cur_scale, MOTION.cx, MOTION.cy, 0);
            }
            _ => {
                // 应用已进前台态（启动转场刚完成/恢复完成）：显存归应用所有，
                // launcher 不得再画桌面覆盖占位页/应用首帧。本帧可能因同帧内
                // 其它路径置位 NEEDS_REDRAW 走到这里（启动完成帧 motion_advance
                // 已改为返回 false，但输入/时钟路径仍可能置位），必须跳过。
                if app_life::app_active() {
                    return;
                }
                draw_desktop_full();
            }
        }
        // 注意：光标不画进 BACK_BUF —— 以 overlay 直接绘制在显存 FB 上
        // （Android 无线鼠标式：移动光标只更新光标所在小区域，不触发整屏重绘）。
    }
}

/// 桌面完整绘制：背景/状态栏/页面/Dock/面板/浮层/导航栏。
/// 正常帧与锁屏解锁/锁定转场（桌面作为锁屏层下方内容）共用。
fn draw_desktop_full() {
    unsafe {
        // 渐进式启动：基底帧（壁纸+状态栏）只画一次，后续批次增量绘制图标。
        if !(BOOT_ACTIVE && BOOT_LIMIT > 0) {
            draw_background();
        }
        draw_status_bar();
        draw_call_banner();
        draw_page();
        draw_dock();
        // 文件夹（含打开/关闭 350ms 弹簧缩放动画）
        if FOLDER_PHASE != FolderPhase::Closed {
            dim_screen();
            if FOLDER_TILE_DIRTY {
                folder_build_tile();
            }
            let s = FOLDER_ANIM.pos;
            folder_blit(s, FOLDER_ANCHOR_X, FOLDER_ANCHOR_Y);
        }
        if RENAME_OPEN {
            draw_rename_panel();
        }
        draw_overlay();
        draw_ripple();
        // 最近任务面板（覆盖桌面，导航栏仍在其上）
        if RECENTS_OPEN {
            draw_recents();
        }
        // 控制中心 / 通知中心（覆盖桌面与最近任务，导航栏之上）。
        control_center::cc_draw();
        draw_nav_bar();
        // 打断动画（ColorOS 撕裂）：应用图标（桌面原位已被隐藏）在飞行位置
        // 撕成上下两半绘制；撕裂进度越大，向白色淡出越明显。
        if MOTION.phase == Phase::FlyAway || MOTION.phase == Phase::FlyBack {
            let app = &APPS[MOTION.app];
            let ix = MOTION.fly_x - ICON_SIZE / 2;
            let iy = MOTION.fly_y - ICON_SIZE / 2;
            let idx = app_index_of(app);
            let split = MOTION.fly_split;
            if split > 1 && idx < APPS.len() && ICON_CACHE_READY[idx] {
                let smax = MOTION.fly_split_max.max(1);
                let fade = ((split * 255) / smax).min(150) as u32;
                blit_icon_split(idx, ix, iy, split, fade);
            } else {
                draw_app_icon(app, ix, iy, ICON_SIZE);
            }
        }
        // 渐进式启动的加载指示器（最上层）。
        if BOOT_ACTIVE {
            draw_boot_progress();
        }
    }
}

/// 鼠标指针：把 tubiao/shubiao.png 贴图 alpha 混合写显存 FB（不经
/// BACK_BUF/flip），颜色先过亮度 LUT 与整屏亮度匹配。
fn draw_cursor_fb() {
    unsafe {
        cursor_sprite::draw_at(
            FB,
            FB_STRIDE_PX,
            FB_W as i32,
            FB_H as i32,
            CURSOR_X,
            CURSOR_Y,
            Some(apply_lut),
        );
    }
}

const CUR_UW: usize = cursor_sprite::CUR_UW as usize;
const CUR_UH: usize = cursor_sprite::CUR_UH as usize;

/// 光标下 26x26 区域的备份（贴图 24x24 + 1px 边距），用于 overlay 移动时擦除旧光标。
static mut CURSOR_UNDER: [u32; CUR_UW * CUR_UH] = [0u32; CUR_UW * CUR_UH];
static mut CUR_UNDER_X: i32 = -1;
static mut CUR_UNDER_Y: i32 = -1;

/// 把 FB 上贴图外扩 1px 的区域保存到 CURSOR_UNDER。
fn save_cursor_under(x: i32, y: i32) {
    unsafe {
        let sx = cursor_sprite::under_x(x);
        let sy = cursor_sprite::under_y(y);
        CUR_UNDER_X = sx;
        CUR_UNDER_Y = sy;
        for dy in 0..CUR_UH as i32 {
            for dx in 0..CUR_UW as i32 {
                let fx = sx + dx;
                let fy = sy + dy;
                let v = if fx >= 0 && fy >= 0 && fx < FB_W as i32 && fy < FB_H as i32 {
                    *FB.add(fy as usize * FB_STRIDE_PX + fx as usize)
                } else {
                    0
                };
                CURSOR_UNDER[(dy * CUR_UW as i32 + dx) as usize] = v;
            }
        }
    }
}

/// 把 CURSOR_UNDER 写回 FB（擦除旧光标，恢复桌面像素）。
fn restore_cursor_under() {
    unsafe {
        let x = CUR_UNDER_X;
        let y = CUR_UNDER_Y;
        for dy in 0..CUR_UH as i32 {
            for dx in 0..CUR_UW as i32 {
                let fx = x + dx;
                let fy = y + dy;
                if fx >= 0 && fy >= 0 && fx < FB_W as i32 && fy < FB_H as i32 {
                    *FB.add(fy as usize * FB_STRIDE_PX + fx as usize) =
                        CURSOR_UNDER[(dy * CUR_UW as i32 + dx) as usize];
                }
            }
        }
    }
}

/// 光标移动快速路径：只更新 FB 上光标所在小区域，不重绘整屏
/// （参考 Android 无线鼠标：光标作为独立 overlay 移动）。
///
/// 脏区域合成器下：恢复旧位置桌面像素 → 画新位置 → 提交 26×26 小脏区
/// present（不拷贝 BACK_BUF，光标像素直接在画布上，内核按脏区拷贝上屏）。
fn move_cursor_fb() {
    unsafe {
        if !MOUSE_SEEN || FB.is_null() {
            CURSOR_DIRTY = false;
            return;
        }
        let (ox, oy) = (CUR_UNDER_X, CUR_UNDER_Y);
        if ox >= 0 {
            mark_dirty(
                ox,
                oy,
                cursor_sprite::CUR_UW as u32,
                cursor_sprite::CUR_UH as u32,
            );
        }
        restore_cursor_under();
        save_cursor_under(CURSOR_X, CURSOR_Y);
        draw_cursor_fb();
        mark_dirty(
            cursor_sprite::under_x(CURSOR_X),
            cursor_sprite::under_y(CURSOR_Y),
            cursor_sprite::CUR_UW as u32,
            cursor_sprite::CUR_UH as u32,
        );
        fb_submit_present();
        CURSOR_DIRTY = false;
    }
}

/// 整屏重绘后把光标以 overlay 画回画布并立即提交（小脏区 present）。
/// 状态栏局部快速重绘（分钟变化路径）：只重绘顶部 STATUS_H 行并标记脏矩形。
///
/// 与整屏 render_frame() 的分支差别：不重画背景波形 / 网格 / Dock / 毛玻璃
/// 面板，不整屏 flip —— flip 只拷贝 (0,0,W,STATUS_H) 矩形，节省每帧 2MB
/// 整屏拷贝 + 逐像素亮度 LUT。光标若在顶部区域内，重绘后由
/// draw_cursor_overlay() 重新叠画，避免脏区覆盖吃掉光标。
fn redraw_status_bar_fast() {
    unsafe {
        draw_status_bar();
        mark_dirty(0, 0, DISPLAY_W as u32, STATUS_H as u32);
        flip();
        draw_cursor_overlay();
        STATUS_DIRTY = false;
    }
}

/// 首页时钟小组件局部重绘（每秒秒针更新走这里，不再触发整屏 render_frame）。
///
/// 此前该 tick 置 NEEDS_REDRAW → 每秒一次整屏重绘（壁纸 + 毛玻璃 + 图标 +
/// TTF 全量渲染，TCG 下 50~150ms），期间主循环被占死，鼠标每秒顿一下 ——
/// 这是「桌面鼠标非常卡」的主因。现在只重绘小组件矩形：壁纸背景（矩形内，
/// 供玻璃取样）→ 玻璃卡片 + 表盘 → 脏矩形 flip 呈现，成本 ~5ms。
fn redraw_clock_widget_fast() -> bool {
    unsafe {
        if FB.is_null() {
            return false;
        }
        let cols = GRID_COLS;
        let mut placed = [PlacedItem::zero(); MAX_PAGE_ITEMS];
        let n = layout_page(&PAGES[CURRENT_PAGE], cols, &mut placed);
        for i in 0..n {
            let p = placed[i];
            if let ItemKind::Widget(WidgetKind::Clock) = p.kind {
                let (wx, wy, ww, wh) = item_rect(&p, cols);
                let (wc, hc) = item_size(p.kind);
                draw_background_rect(wx, wy, ww as u32, wh as u32);
                draw_widget(WidgetKind::Clock, wx, wy, ww, wh, wc, hc);
                mark_dirty(wx, wy, ww as u32, wh as u32);
                flip();
                // flip 的脏矩形拷贝会覆盖画布上的光标 overlay：光标落在
                // 小组件矩形内时必须补画（外部时画布光标仍在，不能重画 ——
                // 否则 save_under 会把光标自身像素存成背景，留下拖影）。
                let ux = cursor_sprite::under_x(CURSOR_X);
                let uy = cursor_sprite::under_y(CURSOR_Y);
                if MOUSE_SEEN
                    && ux < wx + ww
                    && ux + cursor_sprite::CUR_UW > wx
                    && uy < wy + wh
                    && uy + cursor_sprite::CUR_UH > wy
                {
                    draw_cursor_overlay();
                }
                return true;
            }
        }
        false
    }
}

fn draw_cursor_overlay() {
    unsafe {
        if !MOUSE_SEEN || FB.is_null() {
            return;
        }
        save_cursor_under(CURSOR_X, CURSOR_Y);
        draw_cursor_fb();
        mark_dirty(
            cursor_sprite::under_x(CURSOR_X),
            cursor_sprite::under_y(CURSOR_Y),
            cursor_sprite::CUR_UW as u32,
            cursor_sprite::CUR_UH as u32,
        );
        fb_submit_present();
    }
}

/// 亮度 LUT（与 flip() 的 LUT 路径一致），用于 overlay 光标颜色与整屏亮度匹配。
fn apply_lut(c: u32) -> u32 {
    unsafe {
        if BRIGHTNESS >= 100 {
            return c;
        }
        let r = BRIGHTNESS_LUT[((c >> 16) & 0xFF) as usize];
        let g = BRIGHTNESS_LUT[((c >> 8) & 0xFF) as usize];
        let b = BRIGHTNESS_LUT[(c & 0xFF) as usize];
        (r << 16) | (g << 8) | b
    }
}

/// 统计 back buffer 中非零像素数（校验渲染结果被真实写入）。
fn count_pixels() -> usize {
    let mut n = 0usize;
    unsafe {
        for i in 0..(DISPLAY_W * DISPLAY_H) {
            if BACK_BUF[i] != 0 {
                n += 1;
            }
        }
    }
    n
}

// ==================== 进程原语 ====================

/// fork：经 clone(SIGCHLD) 实现；成功父进程返回子 pid，子进程返回 0，失败返回负 errno。
unsafe fn fork() -> isize {
    syscall5(SYS_CLONE, SIGCHLD, 0, 0, 0, 0) as isize
}

/// execve：替换当前进程映像；失败返回负 errno。
unsafe fn execve(path: &[u8], argv: *const usize, envp: *const usize) -> isize {
    syscall3(
        SYS_EXECVE,
        path.as_ptr() as usize,
        argv as usize,
        envp as usize,
    ) as isize
}

/// waitpid：wait4 阻塞等待指定子进程退出，返回回收的 pid；失败返回负 errno。
unsafe fn waitpid(pid: isize, status: &mut i32) -> isize {
    syscall4(SYS_WAIT4, pid as usize, status as *mut i32 as usize, 0, 0) as isize
}

// ==================== 输入处理 ====================

/// 命中测试：返回被点击的桌面项（当前页）。
fn hit_item(x: i32, y: i32) -> Option<PlacedItem> {
    unsafe {
        layout_current();
        let cols = GRID_COLS;
        for i in 0..PLACED_COUNT {
            let p = PLACED[i];
            if DRAG_ACTIVE && DRAG_ITEM == p.index {
                continue;
            }
            let (ix, iy, iw, ih) = item_rect(&p, cols);
            if x >= ix && x < ix + iw && y >= iy && y < iy + ih {
                return Some(p);
            }
        }
    }
    None
}

/// Dock 命中测试：返回应用索引。
fn hit_dock(x: i32, y: i32) -> Option<usize> {
    unsafe {
        for i in 0..DOCK_COUNT {
            let (dx, dy) = dock_rect(i);
            if x >= dx && x < dx + DOCK_ICON && y >= dy && y < dy + DOCK_ICON {
                return Some(DOCK_APPS[i]);
            }
        }
    }
    None
}

fn hit_launchable(x: i32, y: i32) -> Option<usize> {
    if let Some(p) = hit_item(x, y) {
        if let ItemKind::App(app) = p.kind {
            return Some(app);
        }
    }
    hit_dock(x, y)
}

// ==================== 最近任务 ====================

const RECENTS_CARD_W: i32 = 140;
const RECENTS_CARD_H: i32 = 150;
const RECENTS_TOP: i32 = 150;
const RECENTS_GAP_Y: i32 = 20;

/// 重建最近任务列表：最近打开的应用优先，随后按可见应用顺序补足（去重，最多 6 个）。
fn rebuild_recents() {
    unsafe {
        let mut n = 0usize;
        if RECENT_APP != usize::MAX && RECENT_APP < APPS.len() {
            RECENTS_LIST[0] = RECENT_APP;
            n = 1;
        }
        for vi in 0..VISIBLE_COUNT {
            if n >= RECENTS_LIST.len() {
                break;
            }
            let app = VISIBLE[vi];
            let mut dup = false;
            for i in 0..n {
                if RECENTS_LIST[i] == app {
                    dup = true;
                    break;
                }
            }
            if !dup {
                RECENTS_LIST[n] = app;
                n += 1;
            }
        }
        RECENTS_COUNT = n;
    }
}

/// 最近任务卡片布局（与 draw_recents 一致）。
fn recents_card_rect(i: usize) -> (i32, i32) {
    const RECENTS_COLS: i32 = 3;
    const RECENTS_GAP_X: i32 = 24;
    let col = (i as i32) % RECENTS_COLS;
    let row = (i as i32) / RECENTS_COLS;
    let x = (DISPLAY_W as i32
        - (RECENTS_COLS * RECENTS_CARD_W + (RECENTS_COLS - 1) * RECENTS_GAP_X))
        / 2
        + col * (RECENTS_CARD_W + RECENTS_GAP_X);
    let y = RECENTS_TOP + row * (RECENTS_CARD_H + RECENTS_GAP_Y);
    (x, y)
}

/// 最近任务卡片命中测试。
fn recents_hit(x: i32, y: i32) -> Option<usize> {
    unsafe {
        for i in 0..RECENTS_COUNT {
            let (cx, cy) = recents_card_rect(i);
            if x >= cx && x < cx + RECENTS_CARD_W && y >= cy && y < cy + RECENTS_CARD_H {
                return Some(RECENTS_LIST[i]);
            }
        }
    }
    None
}

/// 绘制最近任务面板：全屏压暗 + 液态玻璃卡片网格（图标 + 名称）。
/// 动效（对标 ColorOS 15）：打开时卡片按槽位错峰上滑入场；关闭时整体下滑
/// + 压暗淡出；卡片可按住上拖清理（跟手 → 上飞出屏 → 其余卡片补位）。
fn draw_recents() {
    unsafe {
        let now = now_us();
        // 关闭进度：压暗与内容随进度回落（150ms EaseOut）。
        let close_e = if RECENTS_CLOSING {
            Easing::EaseOut.sample((now.saturating_sub(RECENTS_CLOSE_T0)) as f32 / 150_000.0)
        } else {
            0.0
        };
        let dim = (170.0 * (1.0 - close_e)) as u32;
        if dim > 0 {
            // 全屏压暗（在已绘制的桌面上混合，随关闭淡出）。
            for i in 0..DISPLAY_W * DISPLAY_H {
                BACK_BUF[i] = blend(BACK_BUF[i], 0x00_05_0A_14, dim);
            }
        }
        let drop = (12.0 * close_e) as i32;
        let title = txt("RECENT APPS", "最近任务");
        draw_text(24, 96 + drop, title, COLOR_TEXT);
        // 补位进度（0..1，220ms EaseOut）。
        let shift_e = if REC_SHIFT_T0 > 0 {
            Easing::EaseOut.sample((now.saturating_sub(REC_SHIFT_T0)) as f32 / 220_000.0)
        } else {
            1.0
        };
        // 回弹中的卡位移（200ms EaseOut 回 0）。
        let back_off = if REC_BACK {
            let p = (now.saturating_sub(REC_BACK_T0)) as f32 / 200_000.0;
            (REC_BACK_FROM as f32 * (1.0 - Easing::EaseOut.sample(p.min(1.0)))) as i32
        } else {
            0
        };
        for i in 0..RECENTS_COUNT {
            let app_idx = RECENTS_LIST[i];
            let app = &APPS[app_idx];
            let (bx, by) = recents_card_rect(i);
            let mut cx = bx;
            let mut cy = by;
            // 入场：按槽位错峰（每卡延迟 30ms），上滑 24px + 200ms EaseOut。
            let enter_t = (now.saturating_sub(RECENTS_T0)) as f32 / 200_000.0 - i as f32 * 0.15;
            let enter_e = if enter_t <= 0.0 {
                0.0
            } else {
                Easing::EaseOut.sample(enter_t.min(1.0))
            };
            cy += (24.0 * (1.0 - enter_e)) as i32;
            // 补位：从移除前的旧格位插值到新格位（x/y 同步）。
            if shift_e < 1.0 && REC_REMOVED_SLOT != usize::MAX {
                let old = if i < REC_REMOVED_SLOT { i } else { i + 1 };
                let (fx, fy) = REC_SHIFT_FROM[old];
                cx = fx + (((bx - fx) as f32) * shift_e) as i32;
                cy += (((by - fy) as f32) * shift_e) as i32;
            }
            // 跟手清理 / 回弹位移。
            if REC_DRAG_SLOT == i {
                cy += REC_DRAG_Y;
            } else if REC_BACK && REC_BACK_SLOT == i {
                cy += back_off;
            }
            cy += drop;
            // 液态玻璃卡片（模糊压暗的桌面 + 圆角 + 高光描边）。
            draw_glass(cx, cy, RECENTS_CARD_W, RECENTS_CARD_H, 18);
            let ix = cx + (RECENTS_CARD_W - ICON_SIZE) / 2;
            let iy = cy + 24;
            draw_app_icon(app, ix, iy, ICON_SIZE);
            let name = app.display_name().as_bytes();
            let tx = cx + (RECENTS_CARD_W - text_width(name)) / 2;
            draw_text(tx, iy + ICON_SIZE + 10, name, COLOR_TEXT_MAIN);
        }
        // 上飞中的卡最后绘制（位于最上层，200ms FastOutLinearIn 上飞出屏）。
        if REC_FLY_APP != usize::MAX {
            let p = (now.saturating_sub(REC_FLY_T0)) as f32 / 200_000.0;
            let e = Easing::FastOutLinearIn.sample(p.min(1.0));
            let cy = REC_FLY_Y - (360.0 * e) as i32;
            if cy + RECENTS_CARD_H > 0 {
                let app = &APPS[REC_FLY_APP];
                draw_glass(REC_FLY_X, cy, RECENTS_CARD_W, RECENTS_CARD_H, 18);
                let ix = REC_FLY_X + (RECENTS_CARD_W - ICON_SIZE) / 2;
                let iy = cy + 24;
                draw_app_icon(app, ix, iy, ICON_SIZE);
                let name = app.display_name().as_bytes();
                let tx = REC_FLY_X + (RECENTS_CARD_W - text_width(name)) / 2;
                draw_text(tx, iy + ICON_SIZE + 10, name, COLOR_TEXT_MAIN);
            }
        }
        draw_text(
            24,
            132 + ((RECENTS_COUNT + 2) / 3) as i32 * (RECENTS_CARD_H + RECENTS_GAP_Y) + drop,
            txt("Tap a card to open", "点击卡片打开应用"),
            0x00_78_85_98,
        );
    }
}

// ==================== 最近任务动效控制 ====================

/// 打开最近任务面板（带卡片错峰入场动画）。
fn recents_open() {
    unsafe {
        RECENTS_OPEN = true;
        RECENTS_CLOSING = false;
        rebuild_recents();
        RECENTS_T0 = now_us();
        REC_DRAG_SLOT = usize::MAX;
        REC_FLY_APP = usize::MAX;
        REC_BACK = false;
        REC_BACK_SLOT = usize::MAX;
        REC_SHIFT_T0 = 0;
        REC_REMOVED_SLOT = usize::MAX;
        NEEDS_REDRAW = true;
    }
}

/// 关闭最近任务面板（150ms 下滑 + 压暗淡出，完成后才置 RECENTS_OPEN=false）。
fn recents_close() {
    unsafe {
        if !RECENTS_OPEN || RECENTS_CLOSING {
            return;
        }
        REC_DRAG_SLOT = usize::MAX;
        RECENTS_CLOSING = true;
        RECENTS_CLOSE_T0 = now_us();
        NEEDS_REDRAW = true;
    }
}

/// 最近任务卡槽命中测试（返回槽位索引）。
fn recents_slot_at(x: i32, y: i32) -> Option<usize> {
    unsafe {
        for i in 0..RECENTS_COUNT {
            let (cx, cy) = recents_card_rect(i);
            if x >= cx && x < cx + RECENTS_CARD_W && y >= cy && y < cy + RECENTS_CARD_H {
                return Some(i);
            }
        }
    }
    None
}

/// 卡片清理上飞：记录起飞位置 → 从列表移除 → 启动其余卡片补位动画。
fn begin_rec_fly(slot: usize, drag_y: i32) {
    unsafe {
        let (cx, cy) = recents_card_rect(slot);
        REC_FLY_APP = RECENTS_LIST[slot];
        REC_FLY_X = cx;
        REC_FLY_Y = cy + drag_y;
        REC_FLY_T0 = now_us();
        REC_REMOVED_SLOT = slot;
        let mut i = 0usize;
        while i < RECENTS_COUNT {
            REC_SHIFT_FROM[i] = recents_card_rect(i);
            i += 1;
        }
        let mut j = slot;
        while j + 1 < RECENTS_COUNT {
            RECENTS_LIST[j] = RECENTS_LIST[j + 1];
            j += 1;
        }
        RECENTS_COUNT -= 1;
        REC_SHIFT_T0 = now_us();
        NEEDS_REDRAW = true;
    }
}

/// 跟手清理松手结算：越过阈值（-80px）上飞，否则回弹。
fn recents_drag_settle() {
    unsafe {
        if REC_DRAG_SLOT == usize::MAX {
            return;
        }
        let (slot, y) = (REC_DRAG_SLOT, REC_DRAG_Y);
        REC_DRAG_SLOT = usize::MAX;
        if y < -80 {
            begin_rec_fly(slot, y);
        } else {
            REC_BACK = true;
            REC_BACK_SLOT = slot;
            REC_BACK_FROM = y;
            REC_BACK_T0 = now_us();
            NEEDS_REDRAW = true;
        }
    }
}

/// 最近任务动效推进。返回 true 表示动画进行中（需要继续重绘）。
fn recents_advance(now: u64) -> bool {
    unsafe {
        if !RECENTS_OPEN {
            return false;
        }
        let mut anim = false;
        if RECENTS_CLOSING {
            if now.saturating_sub(RECENTS_CLOSE_T0) >= 150_000 {
                RECENTS_OPEN = false;
                RECENTS_CLOSING = false;
                NEEDS_REDRAW = true;
            } else {
                anim = true;
            }
        }
        if REC_FLY_APP != usize::MAX {
            if now.saturating_sub(REC_FLY_T0) >= 200_000 {
                REC_FLY_APP = usize::MAX;
                NEEDS_REDRAW = true;
            } else {
                anim = true;
            }
        }
        if REC_BACK {
            if now.saturating_sub(REC_BACK_T0) >= 200_000 {
                REC_BACK = false;
                REC_BACK_SLOT = usize::MAX;
                NEEDS_REDRAW = true;
            } else {
                anim = true;
            }
        }
        if REC_SHIFT_T0 > 0 {
            if now.saturating_sub(REC_SHIFT_T0) >= 220_000 {
                REC_SHIFT_T0 = 0;
                REC_REMOVED_SLOT = usize::MAX;
                NEEDS_REDRAW = true;
            } else {
                anim = true;
            }
        }
        // 入场错峰窗口（每卡延迟 30ms + 200ms）。
        if !RECENTS_CLOSING
            && now.saturating_sub(RECENTS_T0) < RECENTS_COUNT as u64 * 30_000 + 200_000
        {
            anim = true;
        }
        anim
    }
}

/// 系统导航栏绘制（三键 / 手势指示条，始终位于最上层，液态玻璃底）。
fn draw_nav_bar() {
    struct LauncherCanvas;
    impl NavCanvas for LauncherCanvas {
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
    let mut cv = LauncherCanvas;
    sys_nav::draw_nav_bar(&mut cv, nav_mode());
}

fn remove_app(app_idx: usize) {
    if app_idx >= APPS.len() || !APPS[app_idx].default_on {
        show_toast(txt("Built-in app cannot be removed", "内置应用不可移除"));
        return;
    }
    unsafe {
        let mut buf = [0u8; 512];
        let n = read_file(REMOVED_PATH, &mut buf);
        let mut used = if n > 0 { n as usize } else { 0 };
        if used > 0 && used < buf.len() - 1 && buf[used - 1] != b',' {
            buf[used] = b',';
            used += 1;
        }
        let key = APPS[app_idx].key.as_bytes();
        if used + key.len() < buf.len() {
            buf[used..used + key.len()].copy_from_slice(key);
            used += key.len();
            if !write_file(REMOVED_PATH, &buf[..used]) {
                show_toast(txt(
                    "Failed to save, app not removed",
                    "保存失败，应用未移除",
                ));
                return;
            }
            filter_modules();
            CURRENT_PAGE = CURRENT_PAGE.min(page_count() - 1);
            EDIT_MODE = false;
            EDIT_APP = usize::MAX;
            show_toast(txt("App removed", "应用已移除"));
        }
    }
}

/// 轮询 /dev/input/event0 所需的输入事件常量与状态。
///
/// 触摸面板物理分辨率（与 HAL 的 QEMU 配置一致，用于物理→屏幕坐标映射）。
const TOUCH_MAX_X: i32 = 540;
const TOUCH_MAX_Y: i32 = 960;

/// 手势分类阈值。
const TAP_MAX_DIST: i32 = 10; // 点击最大位移（px）
const TAP_MAX_MS: u64 = 500; // 点击最大持续时间（ms）
const SWIPE_MIN_DIST: i32 = 30; // 滑动最小位移（px）
const LONG_PRESS_MS: u64 = 500; // 长按最小持续时间（ms，进入编辑模式）

/// linux input_event 事件类型 / 码。
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const BTN_TOUCH: u16 = 0x14a; // 330
const BTN_LEFT: u16 = 0x110; // 272

/// 鼠标指针状态：位置（屏幕像素，相对移动累积）、是否按下、是否已移动进入系统。
static mut CURSOR_X: i32 = 270;
static mut CURSOR_Y: i32 = 480;
static mut MOUSE_DOWN: bool = false;
static mut MOUSE_SEEN: bool = true; // 默认显示指针，鼠标移动后跟随
static mut CURSOR_DIRTY: bool = false;

/// 触摸手势状态机。
struct Touch {
    pressed: bool,
    press_x: i32,
    press_y: i32,
    cur_x: i32,
    cur_y: i32,
    press_us: u64,
    max_dist: i32,
    /// 抬起事件的内核时间戳（µs），用于精确计算手势时长。
    up_us: u64,
}

static mut TOUCH: Touch = Touch {
    pressed: false,
    press_x: 0,
    press_y: 0,
    cur_x: 0,
    cur_y: 0,
    press_us: 0,
    max_dist: 0,
    up_us: 0,
};

/// 最近一次绝对坐标（ABS_X / ABS_Y 分属两个事件）。
static mut LAST_ABS_X: i32 = 0;
static mut LAST_ABS_Y: i32 = 0;

/// 识别出的手势。
#[derive(Clone, Copy)]
enum Gesture {
    None,
    Tap {
        x: i32,
        y: i32,
    },
    /// 滑动：位移 (dx, dy) 与持续时间（ms，用于打断动画的速度估算）。
    Swipe {
        dx: i32,
        dy: i32,
        dur_ms: u64,
    },
    LongPress {
        x: i32,
        y: i32,
    },
    /// 系统导航动作（导航栏点击或全面屏手势）。
    Nav(NavAction),
}

/// 触摸面板物理坐标 → 屏幕像素坐标（540x960）。
fn map_screen(dev_x: i32, dev_y: i32) -> (i32, i32) {
    let sx = (dev_x * DISPLAY_W as i32 + TOUCH_MAX_X / 2) / TOUCH_MAX_X;
    let sy = (dev_y * DISPLAY_H as i32 + TOUCH_MAX_Y / 2) / TOUCH_MAX_Y;
    (sx, sy)
}

/// 非阻塞读取：返回读到的字节数（isize，负值为 errno）。
unsafe fn read_fd(fd: usize, buf: usize, len: usize) -> isize {
    syscall3(SYS_READ, fd, buf, len) as isize
}

/// 解析 input_event 的内核时间戳（tv_sec + tv_usec，µs）。
fn ev_timestamp_us(buf: &[u8]) -> u64 {
    let sec = i64::from_le_bytes([
        buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
    ]);
    let usec = i64::from_le_bytes([
        buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
    ]);
    (sec.max(0) as u64)
        .saturating_mul(1_000_000)
        .saturating_add(usec.max(0) as u64)
}

/// 处理一段 `input_event` 字节流。
///
/// 返回 `(手势, 指针是否移动)`：本段中至多一个完成的手势；鼠标指针移动时
/// 第二项为 true（需要重绘以更新指针位置）。
fn process_events(buf: &[u8]) -> (Option<Gesture>, bool) {
    let mut result = None;
    let mut moved = false;
    let mut i = 0usize;
    while i + 24 <= buf.len() {
        let etype = u16::from_le_bytes([buf[i + 16], buf[i + 17]]);
        let code = u16::from_le_bytes([buf[i + 18], buf[i + 19]]);
        let value = i32::from_le_bytes([buf[i + 20], buf[i + 21], buf[i + 22], buf[i + 23]]);

        match etype {
            EV_REL => {
                // 鼠标相对移动：累积到指针位置并钳制在屏幕内。
                if code == REL_X {
                    unsafe {
                        CURSOR_X = (CURSOR_X + value).clamp(0, DISPLAY_W as i32 - 1);
                        MOUSE_SEEN = true;
                        CURSOR_DIRTY = true;
                    }
                    moved = true;
                } else if code == REL_Y {
                    unsafe {
                        CURSOR_Y = (CURSOR_Y + value).clamp(0, DISPLAY_H as i32 - 1);
                        MOUSE_SEEN = true;
                        CURSOR_DIRTY = true;
                    }
                    moved = true;
                }
                // 鼠标按住拖动：控制中心滑块跟手。
                if unsafe { MOUSE_DOWN } {
                    let (mx, my) = unsafe { (CURSOR_X, CURSOR_Y) };
                    control_center::cc_drag_move(mx, my);
                }
                // 鼠标拖动同样推进手势轨迹（QEMU virtio-mouse / PS2 相对鼠标场景）：
                // 与触摸路径一致地累计位移，抬起时才能分类出滑动/全面屏手势。
                if unsafe { TOUCH.pressed } {
                    let (cx, cy) = unsafe { (CURSOR_X, CURSOR_Y) };
                    unsafe {
                        TOUCH.cur_x = cx;
                        TOUCH.cur_y = cy;
                        let dx = cx - TOUCH.press_x;
                        let dy = cy - TOUCH.press_y;
                        let d = isqrt(dx * dx + dy * dy);
                        if d > TOUCH.max_dist {
                            TOUCH.max_dist = d;
                        }
                    }
                }
            }
            EV_ABS => unsafe {
                // 单点触摸：ABS_X / ABS_Y；多点（ABS_MT_*）为骨架。
                if code == ABS_X {
                    LAST_ABS_X = value;
                    // 绝对定位指针（virtio-tablet）：光标跟随到该位置
                    CURSOR_X = value.clamp(0, DISPLAY_W as i32 - 1);
                    MOUSE_SEEN = true;
                    CURSOR_DIRTY = true;
                    moved = true;
                } else if code == ABS_Y {
                    LAST_ABS_Y = value;
                    CURSOR_Y = value.clamp(0, DISPLAY_H as i32 - 1);
                    MOUSE_SEEN = true;
                    CURSOR_DIRTY = true;
                    moved = true;
                }
                if TOUCH.pressed {
                    let (sx, sy) = map_screen(LAST_ABS_X, LAST_ABS_Y);
                    TOUCH.cur_x = sx;
                    TOUCH.cur_y = sy;
                    let dx = sx - TOUCH.press_x;
                    let dy = sy - TOUCH.press_y;
                    let d = isqrt(dx * dx + dy * dy);
                    if d > TOUCH.max_dist {
                        TOUCH.max_dist = d;
                    }
                    // 控制中心滑块拖动跟手。
                    control_center::cc_drag_move(sx, sy);
                }
            },
            EV_KEY => {
                if code == BTN_LEFT {
                    // 鼠标左键（QEMU GUI 的 virtio-tablet 点击也以 BTN_LEFT 上报）：
                    // 与 BTN_TOUCH 共用同一套完整手势识别 —— 点击 / 滑动 /
                    // 底部上滑 / 边缘返回 / 长按，使 QEMU 下全面屏手势同样可用。
                    if value == 1 {
                        unsafe {
                            MOUSE_DOWN = true;
                            MOUSE_PRESS_US = now_us();
                            MOUSE_LONG = false;
                            TOUCH.pressed = true;
                            TOUCH.press_x = CURSOR_X;
                            TOUCH.press_y = CURSOR_Y;
                            TOUCH.cur_x = CURSOR_X;
                            TOUCH.cur_y = CURSOR_Y;
                            TOUCH.press_us = ev_timestamp_us(&buf[i..]);
                            TOUCH.max_dist = 0;
                            // 锁屏时图标按压反馈死状态防护：PressIn 抬起路径
                            // 被 handle_lock_input 消费，无 cancel_press 机会 →
                            // 每帧全屏重绘 + 图标卡在按压态。
                            if !control_center::cc_visible() && !LOCKED {
                                on_press(CURSOR_X, CURSOR_Y);
                            }
                            // 控制中心交互（开关按压反馈 / 滑块拖动）。
                            control_center::cc_touch_down(CURSOR_X, CURSOR_Y);
                        }
                    } else if value == 0 && unsafe { MOUSE_DOWN } {
                        unsafe {
                            MOUSE_DOWN = false;
                        }
                        // 鼠标长按抬起：吞掉本次点击（避免长按后误触移除）。
                        if unsafe { MOUSE_LONG } {
                            unsafe {
                                MOUSE_LONG = false;
                            }
                        } else {
                            unsafe {
                                TOUCH.up_us = ev_timestamp_us(&buf[i..]);
                            }
                            // 控制中心滑块拖动结束时吞掉本次手势（不触发点击/滑动）。
                            let dragged = control_center::cc_touch_up();
                            if !dragged {
                                if let Some(g) = finish_touch() {
                                    result = Some(g);
                                }
                            }
                        }
                    }
                } else if code == BTN_TOUCH {
                    if value == 1 {
                        unsafe {
                            let (sx, sy) = map_screen(LAST_ABS_X, LAST_ABS_Y);
                            TOUCH.pressed = true;
                            TOUCH.press_x = sx;
                            TOUCH.press_y = sy;
                            TOUCH.cur_x = sx;
                            TOUCH.cur_y = sy;
                            TOUCH.press_us = ev_timestamp_us(&buf[i..]);
                            TOUCH.max_dist = 0;
                            // 按压反馈 / 编辑模式拖拽开始（锁屏时不进入按压态）
                            if !control_center::cc_visible() && !LOCKED {
                                on_press(sx, sy);
                            }
                            // 控制中心交互（开关按压反馈 / 滑块拖动）。
                            control_center::cc_touch_down(sx, sy);
                        }
                        print("[input] 按下（");
                        print_dec(unsafe { TOUCH.press_x } as u32);
                        print(",");
                        print_dec(unsafe { TOUCH.press_y } as u32);
                        print(")\n");
                    } else if value == 0 {
                        unsafe {
                            TOUCH.up_us = ev_timestamp_us(&buf[i..]);
                        }
                        // 控制中心滑块拖动结束时吞掉本次手势（不触发点击/滑动）。
                        let dragged = control_center::cc_touch_up();
                        if !dragged {
                            result = finish_touch();
                        }
                    }
                }
            }
            // EV_SYN 为帧边界：手势在 BTN_TOUCH / BTN_LEFT 抬起时完成（同一识别路径）。
            _ => {}
        }
        i += 24;
    }
    (result, moved)
}

/// 抬起时分类手势：系统导航（按钮/手势）优先，其次点击 / 滑动 / 长按。
fn finish_touch() -> Option<Gesture> {
    unsafe {
        if !TOUCH.pressed {
            return None;
        }
        TOUCH.pressed = false;
        // 用内核事件时间戳计算手势时长（事件可能成批处理，now_us 会低估）
        let dur_ms = TOUCH.up_us.saturating_sub(TOUCH.press_us) / 1000;
        let dist = TOUCH.max_dist;
        let (x, y) = (TOUCH.cur_x, TOUCH.cur_y);
        // 系统导航优先：三键点击 / 全面屏手势（底部上滑、边缘返回）
        let mode = nav_mode();
        let a = nav_hit(x, y, mode);
        if a != NavAction::None {
            return Some(Gesture::Nav(a));
        }
        let g = classify_swipe(TOUCH.press_x, TOUCH.press_y, x, y, dur_ms, mode);
        if g != NavAction::None {
            return Some(Gesture::Nav(g));
        }
        let g = if dist <= TAP_MAX_DIST {
            if dur_ms >= LONG_PRESS_MS {
                Gesture::LongPress { x, y }
            } else if dur_ms <= TAP_MAX_MS {
                Gesture::Tap { x, y }
            } else {
                Gesture::None
            }
        } else if dist > SWIPE_MIN_DIST {
            Gesture::Swipe {
                dx: TOUCH.cur_x - TOUCH.press_x,
                dy: TOUCH.cur_y - TOUCH.press_y,
                dur_ms,
            }
        } else {
            Gesture::None
        };
        Some(g)
    }
}

/// 处理一个完成的手势：点击启动应用、滑动切页、长按提示。
/// 返回是否发生了需要重绘的界面变化（本次实现中为点击命中图标并拉起应用）。
fn handle_gesture(g: Gesture) -> bool {
    // 锁屏态：所有手势先交给锁屏处理，桌面逻辑完全旁路。
    if unsafe { LOCKED } {
        return handle_lock_input(g);
    }
    // 控制中心 / 通知中心：优先消费手势（角部下拉打开、上滑/点空白关闭）。
    if control_center::cc_handle(g) {
        return true;
    }
    match g {
        Gesture::Tap { x, y } => {
            print("[input] 抬起（");
            print_dec(x as u32);
            print(",");
            print_dec(y as u32);
            print("）→ 检测到点击\n");
            unsafe {
                RIPPLE_X = x;
                RIPPLE_Y = y;
                RIPPLE_START = now_us();
                RIPPLE_ACTIVE = true;
            }
            // 来电/通话横幅：点击打开电话应用（接听或查看通话）。
            if call_banner_hit(x, y) {
                if let Some(i) = app_index_by_key("phone") {
                    print("[banner] 通话 → 打开电话\n");
                    start_launch(i);
                }
                return true;
            }
            // 最近任务面板打开时：点击卡片启动应用，点击空白关闭面板。
            if unsafe { RECENTS_OPEN } {
                if unsafe { RECENTS_CLOSING } {
                    return true;
                }
                if let Some(i) = recents_hit(x, y) {
                    // 启动/恢复转场从所点卡片放大进入（ColorOS/鸿蒙卡片起点语义）。
                    unsafe {
                        for slot in 0..RECENTS_COUNT {
                            if RECENTS_LIST[slot] == i {
                                let (rx, ry) = recents_card_rect(slot);
                                set_launch_anchor(
                                    rx + RECENTS_CARD_W / 2,
                                    ry + RECENTS_CARD_H / 2,
                                );
                                break;
                            }
                        }
                    }
                    recents_close();
                    start_launch(i);
                } else {
                    print("[input] 点击关闭最近任务\n");
                    recents_close();
                }
                return true;
            }
            if y < STATUS_H && x < 120 {
                unsafe {
                    SHOW_BATTERY_PERCENT = !SHOW_BATTERY_PERCENT;
                }
                show_toast(txt(
                    if unsafe { SHOW_BATTERY_PERCENT } {
                        "Battery percentage ON"
                    } else {
                        "Battery percentage OFF"
                    },
                    if unsafe { SHOW_BATTERY_PERCENT } {
                        "电量百分比已开启"
                    } else {
                        "电量百分比已关闭"
                    },
                ));
                return true;
            }
            // 文件夹重命名键盘
            if unsafe { RENAME_OPEN } {
                if y >= KB_Y0 {
                    rename_key_hit(x, y);
                } else {
                    unsafe {
                        RENAME_OPEN = false;
                        FOLDER_TILE_DIRTY = true;
                    }
                }
                return true;
            }
            // 文件夹打开态：面板外点击关闭；点名称重命名；点图标启动应用。
            if unsafe { FOLDER_PHASE != FolderPhase::Closed } {
                let (px, py, pw, ph) = folder_panel_rect();
                if x < px || x >= px + pw || y < py || y >= py + ph {
                    unsafe {
                        FOLDER_LAUNCH = usize::MAX;
                    }
                    folder_start_close();
                    return true;
                }
                if y >= py + 14 && y < py + 62 {
                    rename_open();
                    return true;
                }
                if let Some(app) = folder_hit(x, y) {
                    unsafe {
                        FOLDER_LAUNCH = app;
                    }
                    folder_start_close();
                    return true;
                }
                return true;
            }
            if unsafe { EDIT_MODE } {
                if unsafe { DRAG_ACTIVE } {
                    edit_drop(x, y);
                    return true;
                }
                if grid_toggle_hit(x, y) {
                    toggle_grid();
                    return true;
                }
                if let Some(p) = hit_item(x, y) {
                    match p.kind {
                        ItemKind::App(app) => remove_app(app),
                        ItemKind::Folder => folder_start_open(),
                        _ => {}
                    }
                } else {
                    unsafe {
                        EDIT_MODE = false;
                    }
                }
                return true;
            }
            match hit_item(x, y) {
                Some(p) => match p.kind {
                    ItemKind::App(app) => {
                        start_launch(app);
                    }
                    ItemKind::Folder => folder_start_open(),
                    ItemKind::Widget(k) => widget_launch(k),
                },
                None => {
                    if let Some(app) = hit_dock(x, y) {
                        start_launch(app);
                    } else {
                        cancel_press();
                    }
                }
            }
            true
        }
        Gesture::Swipe { dx, dy, dur_ms } => {
            print("[input] 抬起 → 检测到滑动（");
            print_dec(dx as u32);
            print(",");
            print_dec(dy as u32);
            print(")\n");
            // 打断启动动画：应用启动转场未播完时上划 → 图标飞出、1s 后弹回原位
            if unsafe { MOTION.phase == Phase::Launch } && dy < 0 {
                let app = unsafe { MOTION.app };
                start_fly_away(app, dx, dy, dur_ms);
                return true;
            }
            // 跟手翻页结算：本轮触摸已进入翻页语义，按位移/速度弹簧吸附。
            if unsafe { PAGE_DRAG } {
                let vel = if dur_ms > 0 {
                    dx as f32 / dur_ms as f32 * 1000.0
                } else {
                    0.0
                };
                let (off, w) = unsafe { (PAGE_OFFSET, DISPLAY_W as f32) };
                let dir = if off < -w * 0.25 || vel < -600.0 {
                    1
                } else if off > w * 0.25 || vel > 600.0 {
                    -1
                } else {
                    0
                };
                page_settle(dir, vel);
                return true;
            }
            // 最近任务面板打开时：上滑清理结算 / 忽略其它滑动（不翻页）。
            if unsafe { RECENTS_OPEN } {
                recents_drag_settle();
                return true;
            }
            if unsafe { EDIT_MODE } {
                if unsafe { DRAG_ACTIVE } {
                    let (ex, ey) = unsafe { (TOUCH.cur_x, TOUCH.cur_y) };
                    edit_drop(ex, ey);
                }
                return true;
            }
            if unsafe { FOLDER_PHASE != FolderPhase::Closed || RENAME_OPEN } {
                // 文件夹/重命名打开时禁止切页
                cancel_press();
                return true;
            }
            if dx.abs() > SWIPE_MIN_DIST {
                if unsafe { BOOT_ACTIVE } {
                    // 渐进式启动期间保持瞬切（相邻页图标缓存尚未按需构建）。
                    unsafe {
                        if dx < 0 {
                            CURRENT_PAGE = (CURRENT_PAGE + 1).min(page_count() - 1);
                        } else {
                            CURRENT_PAGE = CURRENT_PAGE.saturating_sub(1);
                        }
                        NEEDS_REDRAW = true;
                    }
                } else {
                    // 翻页动画（ColorOS 15 页面滑动吸附）。
                    page_settle(if dx < 0 { 1 } else { -1 }, 0.0);
                    return true;
                }
            }
            cancel_press();
            true
        }
        Gesture::LongPress { x, y } => {
            print("[input] 长按（");
            print_dec(x as u32);
            print(",");
            print_dec(y as u32);
            print(")\n");
            unsafe {
                if EDIT_MODE {
                    if DRAG_ACTIVE {
                        let (ex, ey) = (TOUCH.cur_x, TOUCH.cur_y);
                        edit_drop(ex, ey);
                    }
                } else if FOLDER_PHASE == FolderPhase::Closed && !RENAME_OPEN {
                    // 长按 500ms 进入编辑模式（仅应用图标）。
                    if let Some(p) = hit_item(x, y) {
                        if let ItemKind::App(_) = p.kind {
                            EDIT_MODE = true;
                            show_toast(txt(
                                "Edit mode: drag to move, tap to remove",
                                "编辑模式：拖动移动，点击移除",
                            ));
                            NEEDS_REDRAW = true;
                        }
                    }
                }
            }
            cancel_press();
            true
        }
        Gesture::Nav(a) => {
            print("[input] 导航操作\n");
            // 启动转场未播完时上划（手势模式底部上滑=桌面）同样打断启动：
            // 以等效的上滑手势让图标飞出。
            if unsafe { MOTION.phase == Phase::Launch }
                && (a == NavAction::Home || a == NavAction::Recents)
            {
                let app = unsafe { MOTION.app };
                start_fly_away(app, 0, -160, 180);
                return true;
            }
            // 跟手翻页/清理拖动被导航手势打断：直接结算，避免状态残留。
            if unsafe { PAGE_DRAG } {
                page_settle(0, 0.0);
            }
            if unsafe { REC_DRAG_SLOT != usize::MAX } {
                recents_drag_settle();
            }
            unsafe {
                match a {
                    NavAction::Back => {
                        if FOLDER_PHASE != FolderPhase::Closed {
                            FOLDER_LAUNCH = usize::MAX;
                            folder_start_close();
                        } else if RENAME_OPEN {
                            RENAME_OPEN = false;
                            FOLDER_TILE_DIRTY = true;
                        } else {
                            // 桌面无上级页面：提示已在桌面。
                            show_toast(txt("Already on home screen", "已在桌面"));
                        }
                    }
                    NavAction::Home => {
                        CURRENT_PAGE = 0;
                        SHADE_OPEN = false;
                        recents_close();
                        EDIT_MODE = false;
                        RENAME_OPEN = false;
                        FOLDER_PHASE = FolderPhase::Closed;
                        FOLDER_LAUNCH = usize::MAX;
                    }
                    NavAction::Recents => {
                        FOLDER_PHASE = FolderPhase::Closed;
                        FOLDER_LAUNCH = usize::MAX;
                        RENAME_OPEN = false;
                        if RECENTS_OPEN {
                            recents_close();
                        } else {
                            recents_open();
                        }
                        SHADE_OPEN = false;
                    }
                    NavAction::None => {}
                }
            }
            cancel_press();
            true
        }
        Gesture::None => false,
    }
}

/// 排空 /dev/input/event0 的非阻塞事件流，逐段解析并处理手势。
/// 返回本批事件中是否产生了需要重绘的变化。
///
/// 注意：鼠标移动不再触发整屏重绘 —— process_events 只置 CURSOR_DIRTY，
/// 主循环走 `move_cursor_fb`（overlay 快速路径），只有手势/状态变化才整屏重绘。
fn handle_input(input_fd: isize) -> bool {
    let mut redraw = false;
    loop {
        // 256B（10 个 input_event）：快速滑动时每帧多次 2 事件的小读改为
        // 一批，evdev 保证整事件返回，无半包风险。
        let mut buf = [0u8; 256];
        let n = unsafe { read_fd(input_fd as usize, buf.as_mut_ptr() as usize, buf.len()) };
        if n <= 0 {
            break; // EAGAIN（无事件）或读取错误
        }
        let (g, _moved) = process_events(&buf[..n as usize]);
        if let Some(g) = g {
            if handle_gesture(g) {
                redraw = true;
            }
        }
    }
    redraw
}

/// Settings 图标中心（供固定坐标模拟点击）。
fn settings_icon_center() -> (i32, i32) {
    unsafe {
        for i in 0..DOCK_COUNT {
            if APPS[DOCK_APPS[i]].key == "settings" {
                let (dx, dy) = dock_rect(i);
                return (dx + DOCK_ICON / 2, dy + DOCK_ICON / 2);
            }
        }
    }
    (DISPLAY_W as i32 / 2, DISPLAY_H as i32 / 2)
}

// ==================== 运动 / 动画（对标 iOS + Android） ====================
//
// 与内核 kunxcore/src/anim.rs 遵循同一运动规范（数值见 docs/animations.md）：
//   - 缓动曲线：iOS Core Animation（EaseIn/EaseOut/EaseInEaseOut）
//     + Android Material（FastOutSlowIn/LinearOutSlowIn/FastOutLinearIn/Emphasized）
//   - 弹簧：阻尼谐振子（半隐式欧拉积分，无 exp 依赖）
//   - 交互：图标按压反馈（0.85 缩放）+ 应用启动缩放转场（iOS app-open 风格）
//     + 返回缩放转场（iOS app-dismiss 风格）

/// 定点缩放：1.0 = 256。
const SCALE_1: u32 = 256;
/// 编辑模式图标晃动正弦表（16 项，±100 = ±1.0）：平滑摆动替代方波跳变，
/// 每图标相位错开，观感为连续的"轻颤"而非"闪烁"。
const SINE16: [i32; 16] = [
    0, 38, 71, 92, 100, 92, 71, 38, 0, -38, -71, -92, -100, -92, -71, -38,
];

/// 缓动曲线（与内核 anim.rs 同规范）。
#[derive(Clone, Copy, PartialEq)]
enum Easing {
    Linear,
    EaseIn,
    EaseOut,
    EaseInEaseOut,
    FastOutSlowIn,
    LinearOutSlowIn,
    FastOutLinearIn,
    Emphasized,
}

impl Easing {
    fn cp(self) -> (f32, f32, f32, f32) {
        match self {
            Easing::Linear => (0.0, 0.0, 1.0, 1.0),
            Easing::EaseIn => (0.42, 0.0, 1.0, 1.0),
            Easing::EaseOut => (0.0, 0.0, 0.58, 1.0),
            Easing::EaseInEaseOut => (0.42, 0.0, 0.58, 1.0),
            Easing::FastOutSlowIn => (0.4, 0.0, 0.2, 1.0),
            Easing::LinearOutSlowIn => (0.0, 0.0, 0.2, 1.0),
            Easing::FastOutLinearIn => (0.4, 0.0, 1.0, 1.0),
            Easing::Emphasized => (0.2, 0.0, 0.0, 1.0),
        }
    }
    fn sample(self, t: f32) -> f32 {
        let (x1, y1, x2, y2) = self.cp();
        solve_bezier(x1, y1, x2, y2, t)
    }
}

#[inline]
fn bezier_sample(a: f32, b: f32, c: f32, d: f32, t: f32) -> f32 {
    let mt = 1.0 - t;
    let mt2 = mt * mt;
    let mt3 = mt2 * mt;
    let t2 = t * t;
    let t3 = t2 * t;
    a * mt3 + 3.0 * b * mt2 * t + 3.0 * c * mt * t2 + d * t3
}

#[inline]
fn bezier_deriv(a: f32, b: f32, c: f32, d: f32, t: f32) -> f32 {
    let mt = 1.0 - t;
    3.0 * mt * mt * (b - a) + 6.0 * mt * t * (c - b) + 3.0 * t * t * (d - c)
}

/// 求贝塞尔 x 曲线在给定 x 处的参数 t（牛顿 + 二分）。
fn solve_x(x1: f32, x2: f32, x: f32) -> f32 {
    let mut t = x;
    let mut converged = false;
    for _ in 0..8 {
        let err = bezier_sample(0.0, x1, x2, 1.0, t) - x;
        if err.abs() < 1e-4 {
            converged = true;
            break;
        }
        let d = bezier_deriv(0.0, x1, x2, 1.0, t);
        if d.abs() < 1e-6 {
            break;
        }
        t -= err / d;
    }
    if converged {
        return t.clamp(0.0, 1.0);
    }
    // 二分细化；用牛顿结果收窄区间（每 tween 每帧省 ~8 次 bezier 求值）。
    let (mut lo, mut hi) = if t > x {
        (0.0f32, t.clamp(0.0, 1.0))
    } else {
        (t.clamp(0.0, 1.0), 1.0f32)
    };
    for _ in 0..12 {
        let m = (lo + hi) * 0.5;
        if bezier_sample(0.0, x1, x2, 1.0, m) < x {
            lo = m;
        } else {
            hi = m;
        }
    }
    ((lo + hi) * 0.5).clamp(0.0, 1.0)
}

fn solve_bezier(x1: f32, y1: f32, x2: f32, y2: f32, x: f32) -> f32 {
    bezier_sample(0.0, y1, y2, 1.0, solve_x(x1, x2, x))
}

fn fsqrt(x: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    let mut r = x * 0.5;
    for _ in 0..8 {
        r = 0.5 * (r + x / r);
    }
    r
}

/// 应用图标中心（桌面页优先，其次 Dock；文件夹内应用返回文件夹锚点）。
fn icon_center(app_idx: usize) -> (i32, i32) {
    unsafe {
        let cols = GRID_COLS;
        for page in 0..PAGE_COUNT {
            let mut placed = [PlacedItem::zero(); MAX_PAGE_ITEMS];
            let n = layout_page(&PAGES[page], cols, &mut placed);
            for i in 0..n {
                if let ItemKind::App(a) = placed[i].kind {
                    if a == app_idx {
                        let (x, y, w, h) = item_rect(&placed[i], cols);
                        return (x + w / 2, y + h / 2);
                    }
                }
            }
        }
        for i in 0..DOCK_COUNT {
            if DOCK_APPS[i] == app_idx {
                let (dx, dy) = dock_rect(i);
                return (dx + DOCK_ICON / 2, dy + DOCK_ICON / 2);
            }
        }
        // 文件夹内应用：以文件夹图标中心为启动锚点。
        for i in 0..FOLDER_COUNT {
            if FOLDER_APPS[i] == app_idx {
                let (ax, ay) = folder_icon_center();
                return (ax, ay);
            }
        }
    }
    (DISPLAY_W as i32 / 2, DISPLAY_H as i32 / 2)
}

// ==================== 渐进式启动 ====================

/// 启动第 0 帧：壁纸 + 状态栏 + Dock 底条 + 加载指示器（无图标）。
/// 锁屏态直接渲染锁屏首帧，不做渐进加载。
fn boot_begin() {
    unsafe {
        BOOT_ACTIVE = true;
        BOOT_LIMIT = 0;
        BOOT_TOTAL = PAGES[0].count + DOCK_COUNT;
        BOOT_BUILT = 0;
    }
    render_frame();
    flip();
    // 桌面首帧已渲染：触发内核侧启动画面 150ms 线性淡出（无启动画面时无操作）。
    splash_fade();
    unsafe {
        draw_cursor_overlay();
        if LOCKED {
            BOOT_ACTIVE = false;
        }
    }
}

/// 渐进式图标加载：每批 4 个（先预渲染缓存瓦片再绘制），单帧 < 100ms。
/// 每批完成后翻转一次，图标逐批出现在网格与 Dock 上。
fn boot_run() {
    unsafe {
        if !BOOT_ACTIVE || LOCKED {
            return;
        }
        while BOOT_LIMIT < BOOT_TOTAL {
            BOOT_LIMIT = (BOOT_LIMIT + 4).min(BOOT_TOTAL);
            while BOOT_BUILT < BOOT_LIMIT && BOOT_BUILT < VISIBLE_COUNT {
                build_icon_cache_entry(VISIBLE[BOOT_BUILT]);
                BOOT_BUILT += 1;
            }
            render_frame();
            flip();
            draw_cursor_overlay();
        }
        BOOT_ACTIVE = false;
    }
}

// ==================== 主流程 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    // 门控文件复位为放行态：launcher 是门控创建者，自身 font_init 不应等待；
    // 也避免上次异常退出残留 "0" 导致死等。
    app_launch::gate_reset();
    print("[launcher] KunxOS 桌面启动器启动中\n");

    // 打开 /dev/fb0 并 mmap 到用户空间，直接绘制像素。
    // 首次启动时 OOBE 已在显存预绘桌面基底帧，进程切换期间屏幕保持该帧。
    // 失败原因（open 返回码 / mmap 返回值）已在 fb_init 内分别打印。
    if !fb_init() {
        print("[launcher] 帧缓冲初始化失败，无帧缓冲\n");
        unsafe {
            exit(1);
        }
    }
    print("[launcher] 帧缓冲 ");
    print_dec(unsafe { FB_W } as u32);
    print("x");
    print_dec(unsafe { FB_H } as u32);
    print(" 已映射\n");

    // 加载平滑字体（/fonts/cn.ttf 常规 + /fonts/bold.ttf 标题，整系统文字走 TTF）。
    // 注意：shared/ttf_text.rs 的 brk 分配器已改为「大块缓冲 bump」——单次 brk 拉入
    // 4MB，fontdue 解析期间 ~3 万次逐页 syscall 降为几十次，TCG 宿主下两个字体在
    // 秒级完成（此前逐页版本实测单字体 ~30s，启动看起来「永久卡住」）。
    // 若字体未就绪，文本管线自动回退内置点阵字库（ttf_glyph / ttf_glyph_px →
    // draw_bitmap_scaled），桌面仍可靠渲染。
    // 注：内核侧启动画面 logo 由内核绘制，淡出由本应用首帧后的 splash_fade() 触发。
    ttf_text::font_init_launcher();
    print("[launcher] font_init done (launcher-cn + launcher-bold)\n");
    // 读取桌面应用图标贴图（tools/make_icons.py 生成，随 rootfs 以
    // /share/app_icons.rgba 提供）。缺失/长度不符时 install 返回 false，
    // 图标回退程序化几何绘制，桌面仍可靠渲染。
    let icon_n = unsafe { read_file("/share/app_icons.rgba", &mut ICON_FILE_BUF) };
    if icon_n == app_icons::ALL_BYTES as isize && app_icons::install(unsafe { &ICON_FILE_BUF }) {
        print("[launcher] 图标贴图已加载 (21 apps)\n");
    } else {
        print("[launcher] 图标贴图缺失/损坏，回退程序化图标\n");
    }
    // 读取 /data/oobe/modules 与 modules_removed，构建可见应用列表。
    filter_modules();
    load_preferences();
    // 读取 OOBE 安全配置（PIN/图案/密码哈希 + 远程锁定标志），决定是否锁屏。
    load_security();

    // 渐进式启动：第 0 帧 = 壁纸 + 状态栏 + 加载指示器，随后图标逐批出现。
    boot_begin();
    boot_run();

    // 首帧完整桌面（全部图标 + 小组件），随后进入主循环。
    unsafe {
        NEEDS_REDRAW = true;
    }
    render_frame();
    flip();
    // 首帧后同样以 overlay 形式画上光标。
    draw_cursor_overlay();
    unsafe {
        NEEDS_REDRAW = false;
        LAST_MINUTE = now_us() / 60_000_000;
    }
    print("[launcher] 桌面就绪，");
    print_dec(unsafe { VISIBLE_COUNT } as u32);
    print(" 个应用\n");
    let lit = count_pixels();
    print("[launcher] 已渲染 ");
    print_dec(lit as u32);
    print(" px\n");

    let start = now_us();
    let mut sim_clicked = false;

    // 打开真实输入设备（非阻塞）；失败则回退为固定坐标模拟点击。
    let input_fd = unsafe { open(INPUT_PATH, O_RDONLY | O_NONBLOCK) };
    if input_fd < 0 {
        print("[input] /dev/input/event0 未就绪，回退到模拟点击\n");
    } else {
        print("[input] /dev/input/event0 就绪\n");
    }

    loop {
        // 帧开始时刻（µs）：既是本帧时间戳，也是帧预算的起点
        let frame_start = now_us();
        let now = frame_start;

        // 0. 生命周期：应用前台/恢复期间，launcher 不渲染桌面、不读输入
        //    （应用独占 fb0 与输入设备）；只轮询 wait4(WNOHANG) 与 lifd 事件。
        if app_life::app_active() {
            let changed = poll_foreground();
            if changed {
                unsafe {
                    NEEDS_REDRAW = true;
                }
            }
            // 恢复转场：应用末帧快照从图标放大到全屏（应用仍 SIGSTOP）。
            if app_life::in_resuming() {
                if motion_advance(now) {
                    unsafe {
                        NEEDS_REDRAW = true;
                    }
                }
                if unsafe { NEEDS_REDRAW } {
                    render_frame();
                    flip();
                    unsafe {
                        NEEDS_REDRAW = false;
                    }
                }
            }
            sleep_ms(8);
            continue;
        }
        // 0.5 桌面期间：处理 lifd 事件（LMK 击杀后台应用 → 回收清理）。
        if poll_background_events() {
            unsafe {
                NEEDS_REDRAW = true;
            }
        }
        // 0.6 非阻塞回收被取消启动的应用子进程（上划打断：应用退出时机不可控，
        //     同步 waitpid 会冻结主循环，见 app_launch::start_fly_away）。
        if poll_cancel_reap() {
            unsafe {
                NEEDS_REDRAW = true;
            }
        }

        // 1. 状态栏时钟：分钟变化时只重绘状态栏行（局部脏矩形）。
        //    第一屏的时钟小组件每秒已整屏重绘、天然刷新状态栏；其余页面
        //    状态栏时间变化只影响顶部 24px，走 redraw_status_bar_fast()
        //    局部路径，不做全屏重绘（背景渐变 + 波形 + 图标 + TTF 全部省掉）。
        let minute = now / 60_000_000;
        if unsafe { minute != LAST_MINUTE } {
            unsafe {
                LAST_MINUTE = minute;
                if LOCKED || FOLDER_PHASE != FolderPhase::Closed || RENAME_OPEN {
                    // 覆盖层（锁屏/文件夹/重命名）按原整屏路径
                    NEEDS_REDRAW = true;
                } else if CURRENT_PAGE != 0 {
                    // 其余页面：状态栏行局部重绘
                    STATUS_DIRTY = true;
                } else if RECENTS_OPEN {
                    NEEDS_REDRAW = true;
                } else {
                    // 首页：状态栏 + 时钟小组件都走局部重绘（数字时间/日期
                    // 在小组件内随本次刷新更新）
                    redraw_status_bar_fast();
                    if !redraw_clock_widget_fast() {
                        NEEDS_REDRAW = true;
                    }
                }
            }
        }
        // 1.2 时钟小组件（第一屏，含秒针）每秒刷新；相册小组件每 3s 轮播。
        let sec = now / 1_000_000;
        if unsafe { sec != LAST_SECOND } {
            unsafe {
                LAST_SECOND = sec;
                if CURRENT_PAGE == 0
                    && !LOCKED
                    && FOLDER_PHASE == FolderPhase::Closed
                    && !RENAME_OPEN
                {
                    // 秒针更新走小组件局部重绘（~5ms），不再整屏 render_frame
                    //（整屏路径 TCG 下 50~150ms，每秒卡顿一次即「鼠标非常卡」）。
                    if RECENTS_OPEN || !redraw_clock_widget_fast() {
                        NEEDS_REDRAW = true;
                    }
                }
            }
        }
        let tick = (now / 3_000_000) as usize;
        if unsafe { tick != GALLERY_TICK } {
            unsafe {
                GALLERY_TICK = tick;
                if CURRENT_PAGE == 1
                    && !LOCKED
                    && FOLDER_PHASE == FolderPhase::Closed
                    && !RENAME_OPEN
                {
                    NEEDS_REDRAW = true;
                }
            }
        }
        // 1.4 鼠标长按（500ms）进入编辑模式。
        if unsafe { MOUSE_DOWN && !MOUSE_LONG && now > MOUSE_PRESS_US + LONG_PRESS_MS * 1000 } {
            unsafe {
                MOUSE_LONG = true;
                mouse_long_press(CURSOR_X, CURSOR_Y);
            }
        }

        // 1.5 锁屏时钟分钟刷新 + 远程锁定轮询（云服务下发 lock 命令后，
        // cloud 应用写 /data/oobe/locked=1；launcher 每秒检查一次即时上锁）。
        if unsafe { LOCKED && minute != LOCK_MINUTE } {
            unsafe {
                LOCK_MINUTE = minute;
                NEEDS_REDRAW = true;
            }
        }
        if !unsafe { LOCKED } && now % 1_000_000 < 16_000 {
            let mut buf = [0u8; 4];
            let n = unsafe { read_file(LOCKED_PATH, &mut buf) };
            if n > 0 && buf[0] == b'1' {
                print("[lock] 收到远程锁定请求\n");
                lock_now();
            }
        }

        // 1.6 来电/通话横幅轮询（rild 状态 → 桌面提示，点击打开电话应用）。
        poll_rild_banner();

        // 2. 输入处理：真实输入或固定坐标模拟点击。
        if input_fd >= 0 {
            if handle_input(input_fd) {
                unsafe {
                    NEEDS_REDRAW = true;
                }
            }
        } else if !sim_clicked && now.saturating_sub(start) >= SIM_CLICK_US {
            sim_clicked = true;
            let (sx, sy) = settings_icon_center();
            print("[launcher] 模拟点击（");
            print_dec(sx as u32);
            print(",");
            print_dec(sy as u32);
            print(")\n");
            if let Some(i) = hit_launchable(sx, sy) {
                start_launch(i);
            }
        }

        // 2.4 编辑模式拖拽跟随手指（触控/鼠标按住时实时更新图标位置）。
        if unsafe { DRAG_ACTIVE && (TOUCH.pressed || MOUSE_DOWN) } {
            let (x, y) = unsafe {
                if TOUCH.pressed {
                    (TOUCH.cur_x, TOUCH.cur_y)
                } else {
                    (CURSOR_X, CURSOR_Y)
                }
            };
            unsafe {
                if !DRAG_MOVED {
                    let dx = x - DRAG_X;
                    let dy = y - DRAG_Y;
                    if dx * dx + dy * dy > TAP_MAX_DIST * TAP_MAX_DIST {
                        DRAG_MOVED = true;
                    }
                }
                DRAG_X = x;
                DRAG_Y = y;
                NEEDS_REDRAW = true;
            }
        }

        // 2.45 跟手动效（触控/鼠标按住期间实时跟随）：
        //   桌面横向拖动 → 跟手翻页；最近任务卡片上拖 → 清理跟手；
        //   锁屏（滑动解锁）上拖 → 锁屏层跟手位移。
        if unsafe { TOUCH.pressed } {
            // 跟手翻页：越过阈值（36px 且横向占优）进入本轮翻页语义。
            let (dx, dy) = unsafe { (TOUCH.cur_x - TOUCH.press_x, TOUCH.cur_y - TOUCH.press_y) };
            if unsafe { PAGE_DRAG } {
                page_drag_update(dx);
            } else if page_drag_allowed() && dx.abs() > 36 && dx.abs() > dy.abs() * 2 {
                unsafe {
                    PAGE_DRAG = true;
                    cancel_press();
                }
                page_drag_update(dx);
            }
            // 控制中心/通知中心跟手下拉：顶部区域纵向拖动 → 面板 1:1 跟手
            // （ColorOS/鸿蒙 语义；跟手中优先于翻页，翻页互斥见 page_drag_allowed）。
            // 区域与 flick 打开路径一致：左上（x<120）= 通知中心，
            // 右上（x≥W-50）= 控制中心。
            if unsafe { control_center::shade_drag_active() } {
                control_center::shade_drag_update(dy);
            } else if dy > 16
                && dy > dx.abs()
                && unsafe { TOUCH.press_y <= 50 }
                && unsafe { control_center::shade_drag_allowed() }
            {
                let px = unsafe { TOUCH.press_x };
                let w = DISPLAY_W as i32;
                if px < 120 || px >= w - 50 {
                    if control_center::shade_drag_start(px < 120) {
                        control_center::shade_drag_update(dy);
                    }
                }
            }
            // 最近任务卡片清理跟手。
            if unsafe {
                RECENTS_OPEN
                    && !RECENTS_CLOSING
                    && REC_DRAG_SLOT == usize::MAX
                    && REC_FLY_APP == usize::MAX
                    && !REC_BACK
            } {
                if let Some(slot) =
                    recents_slot_at(unsafe { TOUCH.press_x }, unsafe { TOUCH.press_y })
                {
                    let (rdx, rdy) =
                        unsafe { (TOUCH.cur_x - TOUCH.press_x, TOUCH.cur_y - TOUCH.press_y) };
                    if rdy < -16 && rdy.abs() > rdx.abs() {
                        unsafe {
                            REC_DRAG_SLOT = slot;
                            REC_DRAG_Y = rdy;
                            NEEDS_REDRAW = true;
                        }
                    }
                }
            } else if unsafe { REC_DRAG_SLOT != usize::MAX } {
                unsafe {
                    REC_DRAG_Y = TOUCH.cur_y - TOUCH.press_y;
                    NEEDS_REDRAW = true;
                }
            }
            // 锁屏（滑动解锁）上滑跟手。
            if unsafe {
                LOCKED && LOCK_PHASE == LockPhase::None && LOCK_SEC_TYPE == 0 && !LOCK_DRAG
            } {
                if dy < -16 && dy.abs() > dx.abs() {
                    unsafe {
                        LOCK_DRAG = true;
                    }
                }
            }
            if unsafe { LOCK_DRAG && LOCK_PHASE == LockPhase::None } {
                unsafe {
                    // 下拖给轻微阻尼（0.15x），上拖跟手。
                    LOCK_LAYER_OFF = if dy > 0 { dy as f32 * 0.15 } else { dy as f32 };
                    NEEDS_REDRAW = true;
                }
            }
        } else {
            // 抬手兜底：导航手势等路径未结算的拖动状态在此收敛。
            if unsafe { PAGE_DRAG } {
                let off = unsafe { PAGE_OFFSET };
                let w = DISPLAY_W as f32;
                let dir = if off < -w * 0.25 {
                    1
                } else if off > w * 0.25 {
                    -1
                } else {
                    0
                };
                page_settle(dir, 0.0);
            }
            recents_drag_settle();
            // 控制中心/通知中心跟手下拉抬手结算（位移/速度决策开或回）。
            if control_center::shade_drag_active() {
                control_center::shade_drag_release();
            }
            if unsafe { LOCK_DRAG } {
                let dy = unsafe { TOUCH.cur_y - TOUCH.press_y };
                lock_swipe_settle(dy, 0);
            }
        }

        // 2.5 运动推进：活动动画期间每帧重绘（按压反馈 / 启动转场 / 返回转场
        //     / 文件夹打开关闭 / 控制中心展开收起与按压回弹 / 通知中心 /
        //     编辑模式图标抖动 / 桌面翻页吸附 / 最近任务动效 / 锁屏转场）。
        if motion_advance(now)
            || folder_advance(now)
            || control_center::cc_advance(now)
            || page_advance(now)
            || recents_advance(now)
            || lock_advance(now)
            || wallpaper_fade_active()
            || lock_charging_anim()
            || banner_anim_active()
            || unsafe { RIPPLE_ACTIVE || TOAST_UNTIL > now || EDIT_MODE }
        {
            unsafe {
                NEEDS_REDRAW = true;
            }
        }

        // 3. 仅在需要时重绘：整屏（内容/动画变化）或状态栏局部（分钟变化）。
        //    鼠标移动走 overlay 快速路径（move_cursor_fb），不触发整屏重绘。
        if unsafe { NEEDS_REDRAW } {
            render_frame();
            flip();
            // 整屏重绘后，把光标以 overlay 形式画回显存 FB（小脏区 present）。
            draw_cursor_overlay();
            unsafe {
                NEEDS_REDRAW = false;
            }
            unsafe {
                STATUS_DIRTY = false;
            }
            unsafe {
                CURSOR_DIRTY = false;
            }
        } else if unsafe { STATUS_DIRTY } {
            // 分钟刻度：只重绘顶部 24px 状态栏（脏矩形路径，非整屏）。
            redraw_status_bar_fast();
        } else if unsafe { CURSOR_DIRTY } {
            move_cursor_fb();
        }

        // 4. 帧预算：空闲锁定 16ms（≈60fps）；控制中心动画/拖动期间锁定
        //    8ms（≈120Hz），渲染/输入耗时从睡眠中扣除。
        //    超预算时立即进入下一帧（干净掉帧而非越积越慢），动画期间节奏稳定。
        let elapsed = now_us() - frame_start;
        // 4.5 转场停滞压缩：宿主机高负载 / TCG 卡顿下单帧可能耗时数百毫秒，
        //    启动/返回转场会看起来"卡死"。单帧耗时 >250ms 时把转场时钟提前
        //    相应时间，使转场在 1-2 帧内收敛到完成（应用立即出现），
        //    把可感知的卡顿上界压到 ~250ms。
        if elapsed > 250_000 {
            unsafe {
                match MOTION.phase {
                    Phase::Launch | Phase::Return | Phase::Resume => {
                        MOTION.t0 = MOTION.t0.saturating_add(elapsed - 250_000);
                    }
                    _ => {}
                }
            }
        }
        // 跟手/吸附中的翻页也走高刷新率（拖动流畅度优先）。
        let high_refresh = unsafe { control_center::cc_high_refresh() || PAGE_ANIM || PAGE_DRAG };
        let budget = if high_refresh { 8_000 } else { 16_000 };
        if elapsed < budget {
            let rem = (budget - elapsed) / 1000;
            if rem > 0 {
                sleep_ms(rem as usize);
            }
        }
    }
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
    unsafe {
        write_fd(2, b"PANIC: launcher ");
        if let Some(loc) = info.location() {
            write_fd(2, loc.file().as_bytes());
            write_fd(2, b":");
            let mut buf = [0u8; 12];
            let mut i = buf.len();
            let mut n = loc.line() as u64;
            while n > 0 {
                i -= 1;
                buf[i] = b'0' + (n % 10) as u8;
                n /= 10;
            }
            write_fd(2, &buf[i..]);
        }
        write_fd(2, b"\n");
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
