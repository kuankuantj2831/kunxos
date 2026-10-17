// KunxOS OOBE：首次开机向导（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由内核 init_main 在 displayd 之后加载（/bin/oobe），仅在 /data/oobe/.done
// 不存在时运行。职责：
//   1. 打开 /dev/fb0（openat）并 mmap 到用户空间，在本地 back buffer 上绘制
//      13 步向导（RGBA8888），`flip()` 一次性拷贝到显存并经 FBIO_PRESENT 呈现：
//      欢迎 -> 语言 -> 模块 -> 时区 -> 网络 -> 协议 -> 账号 -> 锁屏方式 -> 密码
//      -> 胁迫密码 -> 胁迫动作 -> 外观 -> 完成
//      版式对标 HyperOS 3 / ColorOS 15：页头为步骤图标（上方偏下）+ 居中标题，
//      选项卡片/输入框 400px 栏宽居中，底部「上一步/下一步」，右上角「跳过」
//      胶囊可放弃当前步直达完成页；所有卡片/输入框/按钮均为圆角。
//   2. 每步点击「下一步」时把该步配置写入 /data/oobe/（openat/write/close，
//      mkdir /data 与 /data/oobe）
//   3. 完成页点击「进入系统」写 .done 后 exit(0)
//
// 输入为 /dev/input/event0 真实触摸事件：每步点击「下一步」才推进，不自动跳过；
// 设备不可用（如 run-gui 无串口触摸）时回退为固定坐标模拟点击自动推进。
// 密码用 FNV-1a 简单哈希，安全哈希后续升级。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;
#[path = "../../../shared/ttf_text.rs"]
mod ttf_text;

// 共享系统导航（用于主题步骤的导航方式预览）。
#[path = "../../../shared/nav.rs"]
mod sys_nav;

// 共享错误提示状态（OOBE 无墙钟，用帧计数计时；横幅绘制见 draw_error_banner）。
#[path = "../../../shared/error_ui.rs"]
mod err_ui;


// 共享 CJK 16x16 点阵字库（GB2312 一级，由 tools/gen_cjk_font.py 生成）。
#[path = "../../../shared/cjk_font_oobe.rs"]
mod cjk_font;

// 系统鼠标指针贴图（tubiao/shubiao.png 生成，24x24 + 热点）。
#[path = "../../../shared/cursor_sprite.rs"]
mod cursor_sprite;

// 统一圆角按钮组件（主/次/禁止三种状态 + 按压反馈），OOBE 所有按钮统一走此组件。
#[path = "widgets/button.rs"]
mod button;

// 共享对话框组件（选择步骤的 OriginOS 风格单选列表卡用）。
#[path = "../../../shared/dialog.rs"]
mod dialog;

// OOBE -> 桌面过渡（预加载 + 进度条 + 资源释放），完成页点击「进入系统」时调用。
mod finish;

// ==================== 系统调用号 ====================

const SYS_READ: usize = 63;
const SYS_WRITE: usize = 64;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_LSEEK: usize = 62;
const SYS_EXIT: usize = 93;
const SYS_NANOSLEEP: usize = 101;
const SYS_MKDIRAT: usize = 34;
const SYS_IOCTL: usize = 29;
const SYS_MMAP: usize = 222;

// ==================== 常量 ====================

const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const O_RDWR: usize = 2;
const O_CREAT: usize = 0x40;
const O_TRUNC: usize = 0x200;
const O_NONBLOCK: usize = 0x800;
const SEEK_END: usize = 2;

/// mmap 保护位与标志。
const PROT_READ: usize = 1;
const PROT_WRITE: usize = 2;
const MAP_SHARED: usize = 1;

/// FBIOGET_VSCREENINFO 请求码（与内核 devfs 约定一致）。
const FBIOGET_VSCREENINFO: usize = 0x4600;

/// FBIO_FADE_SPLASH 请求码：触发内核侧启动画面线性淡出（首帧渲染完成后调用）。
const FBIO_FADE_SPLASH: usize = 0x4601;

/// FBIO_PUT_DAMAGE：应用直绘脏区域提交（[count:u32][rect×count]）。
const FBIO_PUT_DAMAGE: usize = 0x4602;

/// FBIO_PRESENT：应用帧呈现（合成器脏区拷贝 + 覆盖窗口合成 + 翻页）。
const FBIO_PRESENT: usize = 0x4603;

/// 帧缓冲设备路径。
const FB_PATH: &str = "/dev/fb0";

const NET_PIPE_PATH: &str = "/var/run/networkd.pipe";
const INPUT_PATH: &str = "/dev/input/event0";

// ==================== 显示参数 ====================

const DISPLAY_W: usize = 540;
const DISPLAY_H: usize = 960;
const NAV_H: i32 = 84;
const NAV_Y: i32 = DISPLAY_H as i32 - NAV_H;

// ==================== 字体层级与行高 ====================
//
// 系统默认无衬线字体（cn.ttf 平滑渲染；缺失字形回退内置点阵字库，见
// draw_bitmap_scaled_a）：页面标题 28px 加粗；副标题/说明 17px 常规；
// 按钮文字 17px 半加粗；输入框提示 15px 常规 #8E8E93。
// 行高 = 1.4 × 字号；相邻文本元素的 y 坐标差 ≥ 1.5 × 行高（见 GAP_*）。

const FONT_TITLE_PX: f32 = 28.0;
const FONT_BODY_PX: f32 = 17.0;
const FONT_BTN_PX: f32 = 17.0;
const FONT_HINT_PX: f32 = 15.0;
/// 行高（1.4 × 字号，取整）。
const LH_TITLE: i32 = 39; // 28 × 1.4 = 39.2
const LH_BODY: i32 = 24;  // 17 × 1.4 = 23.8
const LH_HINT: i32 = 21;  // 15 × 1.4 = 21.0
/// 相邻文本元素的最小 y 间距（1.5 × 行高，取整）。
const GAP_AFTER_TITLE: i32 = 59; // 1.5 × 39.2
const GAP_AFTER_BODY: i32 = 36;  // 1.5 × 23.8
const GAP_AFTER_HINT: i32 = 32;  // 1.5 × 21.0

// ==================== 页面版式 ====================
//
// HyperOS 3 / ColorOS 15 向导版式：页头 = 步骤图标（渐变绿圆角方块，上方
// 偏下）+ 图标下方居中标题 + 副标题；选项卡片/输入框以 400px 栏宽水平居中；
// 底部为「上一步 / 下一步」；右上角「跳过」胶囊直达完成页。

/// 模块网格通栏区左缘（网格整体水平居中：24 + 240 + 12 + 240 = 516）。
/// 步骤图标（无底座大号矢量字形，~90px 墨迹）垂直中心所在区域顶部 y。
const ICON_Y: i32 = 84;
const ICON_SIZE: i32 = 100;
/// 页面标题（28px 加粗）顶部 y（图标正下方）。
const PAGE_TITLE_Y: i32 = ICON_Y + ICON_SIZE + 24; // 208
/// 副标题/说明（17px 常规）顶部 y。
const PAGE_DESC_Y: i32 = PAGE_TITLE_Y + 42; // 250
/// 输入/选择控件区顶部 y。
const WIDGET_Y: i32 = PAGE_DESC_Y + 44;     // 294

// ==================== 输入框布局 ====================

/// 输入框（render_* 与 tap_* 共用，必须一致）。标签（15px 提示）位于
/// 输入框上方 GAP_AFTER_HINT 处。400px 栏宽水平居中。
const FIELD_X: i32 = 70;
const FIELD_W: i32 = 400;
const FIELD_H: i32 = 48;
const FIELD_Y0: i32 = WIDGET_Y + GAP_AFTER_HINT;                 // 183
const FIELD_Y1: i32 = FIELD_Y0 + FIELD_H + GAP_AFTER_HINT * 2;   // 295

// 底部导航按钮统一由 button 组件绘制（高度固定 50px，宽度按内容自适应），
// 布局见 draw_nav_buttons。
const NAV_BTN_MARGIN: i32 = 12;

// ==================== 模块网格版式 ====================

/// 模块页（render_modules / module_pos / tap_modules 共用，必须一致）。
/// 两列圆角芯片网格（列优先：左列 0..10、右列 11..20），整组水平居中。
const GRID_X: i32 = 24;
const GRID_COL_W: i32 = 240;
const GRID_GAP: i32 = 12;
const GRID_ROW_H: i32 = 44;
const GRID_R: i32 = 12;
const GRID_PITCH: i32 = 52;
const GRID_ROWS_COL: usize = 11;

// ==================== 选择卡片版式 ====================
//
// 语言/时区/锁屏/胁迫动作/外观 等步骤统一「白色圆角卡片 + 右侧单选」，对标
// ColorOS 15 设置向导。选中态 = ColorOS 绿 描边 + 淡绿底 + 绿单选点。
// 400px 栏宽水平居中（(540-400)/2 = 70）。

const CARD_X: i32 = 70;
const CARD_W: i32 = 400;
const CARD_H: i32 = 54;
const CARD_R: i32 = 16;
const CARD_PITCH: i32 = 64;
/// 卡片内右侧控件（单选圈/复选方框）24x24 命中区。
const OPT_RX: i32 = CARD_X + CARD_W - 20 - 24; // 426
const OPT_RY: i32 = 15; // 卡片内垂直居中偏移（(54-24)/2）

// ==================== 颜色（0x00RRGGBB） ====================

// 浅色向导标准配色：白色基底 + 深灰正文 + 绿高亮（色板见 /tmp/light_theme_spec.md）。
const C_TEXT: u32 = 0x00_1A_1C_22;      // 主文字（白底/浅玻璃上）
const C_DIM: u32 = 0x00_6B_72_80;       // 次要/弱化文字
const C_HINT: u32 = 0x00_95_9A_9F;      // 提示文字
const C_ACCENT: u32 = 0x00_00_B9_6B;    // OPPO 绿（单选/描边/进度点）
const C_ACCENT_STRONG: u32 = 0x00_00_8F_53; // 深绿（按压/描边加深）
const C_ACCENT_TINT: u32 = 0x00_E6_F7_EE;   // 选中卡片淡绿底
const C_ACCENT_SOFT: u32 = 0x00_B5_E4_C9;   // 已完成进度点淡绿
const C_CARD_BORDER: u32 = 0x00_D9_DE_E6;   // 卡片描边
const C_RADIO_BORDER: u32 = 0x00_C7_CC_D3;  // 未选中单选圈
const C_BTN_PRIMARY: u32 = 0x00_1A_1C_22;   // 外观步骤「深色」选项预览圆（示例色，非页面主题色）
const C_RED: u32 = 0x00_FF_5A_5A;
const C_GREEN: u32 = 0x00_00_B9_6B;
const C_BTN_BORDER: u32 = 0x00_FF_FF_FF;
const C_WHITE: u32 = 0x00_FF_FF_FF;

// 欢迎页 Hero 徽标（渐变绿圆盘 + K 字）。
const HERO_CX: i32 = DISPLAY_W as i32 / 2; // 270
const HERO_CY: i32 = 288;
const HERO_R: i32 = 44;

// ==================== 动画参数（对标 ColorOS 15，见 docs/animations.md） ====================

/// 动画改为**真实时间推进 + 帧间不睡眠**（帧率拉满，目标 144Hz）：
/// 每帧按 mono_us 计算进度，机器越快帧率越高、时长恒定不变。
const TRANS_DUR_US: u64 = 160_000; // 步间转场
const HERO_DUR_US: u64 = 260_000; // 欢迎页 Hero 弹跳
const TITLE_DUR_US: u64 = 320_000; // 标题区错峰淡入
const PULSE_DUR_US: u64 = 160_000; // 选中卡片脉冲
/// 转场上滑距离（px）。
const TRANS_SLIDE: i32 = 26;

// ==================== 8x8 位图字体 ====================

static FONT_DIGITS: [[u8; 8]; 10] = [
    [0x3C, 0x42, 0x46, 0x4A, 0x52, 0x62, 0x3C, 0x00],
    [0x08, 0x18, 0x28, 0x08, 0x08, 0x08, 0x3E, 0x00],
    [0x3C, 0x42, 0x02, 0x0C, 0x30, 0x40, 0x7E, 0x00],
    [0x3C, 0x42, 0x02, 0x1C, 0x02, 0x42, 0x3C, 0x00],
    [0x0C, 0x14, 0x24, 0x44, 0x7E, 0x04, 0x04, 0x00],
    [0x7E, 0x40, 0x7C, 0x02, 0x02, 0x42, 0x3C, 0x00],
    [0x3C, 0x40, 0x40, 0x7C, 0x42, 0x42, 0x3C, 0x00],
    [0x7E, 0x02, 0x04, 0x08, 0x10, 0x10, 0x10, 0x00],
    [0x3C, 0x42, 0x42, 0x3C, 0x42, 0x42, 0x3C, 0x00],
    [0x3C, 0x42, 0x42, 0x3E, 0x02, 0x02, 0x3C, 0x00],
];

static FONT_COLON: [u8; 8] = [0x00, 0x18, 0x18, 0x00, 0x00, 0x18, 0x18, 0x00];
static FONT_SPACE: [u8; 8] = [0x00; 8];
static FONT_DOT: [u8; 8] = [0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x18, 0x00];
static FONT_DASH: [u8; 8] = [0x00, 0x00, 0x00, 0x00, 0x7E, 0x00, 0x00, 0x00];
static FONT_SLASH: [u8; 8] = [0x02, 0x02, 0x04, 0x08, 0x10, 0x20, 0x20, 0x00];
static FONT_UNDER: [u8; 8] = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x7E];
static FONT_STAR: [u8; 8] = [0x00, 0x14, 0x08, 0x3E, 0x08, 0x14, 0x00, 0x00];
static FONT_COMMA: [u8; 8] = [0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x18, 0x08];
static FONT_LPAREN: [u8; 8] = [0x08, 0x10, 0x20, 0x20, 0x20, 0x10, 0x08, 0x00];
static FONT_RPAREN: [u8; 8] = [0x10, 0x08, 0x04, 0x04, 0x04, 0x08, 0x10, 0x00];
static FONT_QUESTION: [u8; 8] = [0x3C, 0x42, 0x02, 0x0C, 0x10, 0x00, 0x10, 0x00];
static FONT_BANG: [u8; 8] = [0x08, 0x08, 0x08, 0x08, 0x08, 0x00, 0x08, 0x00];
static FONT_EQUALS: [u8; 8] = [0x00, 0x00, 0x7E, 0x00, 0x7E, 0x00, 0x00, 0x00];
static FONT_HASH: [u8; 8] = [0x24, 0x24, 0x7E, 0x24, 0x7E, 0x24, 0x24, 0x00];
static FONT_PERCENT: [u8; 8] = [0x62, 0x64, 0x08, 0x10, 0x26, 0x46, 0x00, 0x00];
static FONT_GT: [u8; 8] = [0x20, 0x10, 0x08, 0x04, 0x08, 0x10, 0x20, 0x00];

static FONT_UPPER: [[u8; 8]; 26] = [
    [0x18, 0x24, 0x42, 0x42, 0x7E, 0x42, 0x42, 0x00],
    [0x7C, 0x42, 0x42, 0x7C, 0x42, 0x42, 0x7C, 0x00],
    [0x3C, 0x42, 0x40, 0x40, 0x40, 0x42, 0x3C, 0x00],
    [0x78, 0x44, 0x42, 0x42, 0x42, 0x44, 0x78, 0x00],
    [0x7E, 0x40, 0x40, 0x7C, 0x40, 0x40, 0x7E, 0x00],
    [0x7E, 0x40, 0x40, 0x7C, 0x40, 0x40, 0x40, 0x00],
    [0x3C, 0x42, 0x40, 0x4E, 0x42, 0x42, 0x3C, 0x00],
    [0x42, 0x42, 0x42, 0x7E, 0x42, 0x42, 0x42, 0x00],
    [0x3E, 0x08, 0x08, 0x08, 0x08, 0x08, 0x3E, 0x00],
    [0x1E, 0x04, 0x04, 0x04, 0x04, 0x44, 0x38, 0x00],
    [0x42, 0x44, 0x48, 0x70, 0x48, 0x44, 0x42, 0x00],
    [0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x7E, 0x00],
    [0x42, 0x66, 0x5A, 0x42, 0x42, 0x42, 0x42, 0x00],
    [0x42, 0x62, 0x52, 0x4A, 0x46, 0x42, 0x42, 0x00],
    [0x3C, 0x42, 0x42, 0x42, 0x42, 0x42, 0x3C, 0x00],
    [0x7C, 0x42, 0x42, 0x7C, 0x40, 0x40, 0x40, 0x00],
    [0x3C, 0x42, 0x42, 0x42, 0x4A, 0x44, 0x3A, 0x00],
    [0x7C, 0x42, 0x42, 0x7C, 0x48, 0x44, 0x42, 0x00],
    [0x3C, 0x42, 0x40, 0x3C, 0x02, 0x42, 0x3C, 0x00],
    [0x7F, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x00],
    [0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x3C, 0x00],
    [0x42, 0x42, 0x42, 0x42, 0x42, 0x24, 0x18, 0x00],
    [0x42, 0x42, 0x42, 0x42, 0x5A, 0x66, 0x42, 0x00],
    [0x42, 0x42, 0x24, 0x18, 0x24, 0x42, 0x42, 0x00],
    [0x41, 0x22, 0x14, 0x08, 0x08, 0x08, 0x08, 0x00],
    [0x7E, 0x02, 0x04, 0x08, 0x10, 0x20, 0x7E, 0x00],
];

static FONT_LOWER: [[u8; 8]; 26] = [
    [0x00, 0x00, 0x3C, 0x02, 0x3E, 0x42, 0x3E, 0x00],
    [0x40, 0x40, 0x7C, 0x42, 0x42, 0x42, 0x7C, 0x00],
    [0x00, 0x00, 0x3C, 0x42, 0x40, 0x42, 0x3C, 0x00],
    [0x02, 0x02, 0x3E, 0x42, 0x42, 0x42, 0x3E, 0x00],
    [0x00, 0x00, 0x3C, 0x42, 0x7E, 0x40, 0x3C, 0x00],
    [0x0E, 0x10, 0x10, 0x3C, 0x10, 0x10, 0x10, 0x00],
    [0x00, 0x00, 0x3E, 0x42, 0x42, 0x3E, 0x02, 0x3C],
    [0x40, 0x40, 0x7C, 0x42, 0x42, 0x42, 0x42, 0x00],
    [0x08, 0x00, 0x18, 0x08, 0x08, 0x08, 0x3E, 0x00],
    [0x04, 0x00, 0x0C, 0x04, 0x04, 0x44, 0x38, 0x00],
    [0x40, 0x40, 0x44, 0x48, 0x70, 0x48, 0x44, 0x00],
    [0x18, 0x08, 0x08, 0x08, 0x08, 0x08, 0x3E, 0x00],
    [0x00, 0x00, 0x66, 0x5A, 0x5A, 0x42, 0x42, 0x00],
    [0x00, 0x00, 0x7C, 0x42, 0x42, 0x42, 0x42, 0x00],
    [0x00, 0x00, 0x3C, 0x42, 0x42, 0x42, 0x3C, 0x00],
    [0x00, 0x00, 0x7C, 0x42, 0x42, 0x7C, 0x40, 0x40],
    [0x00, 0x00, 0x3E, 0x42, 0x42, 0x3E, 0x02, 0x02],
    [0x00, 0x00, 0x5C, 0x62, 0x40, 0x40, 0x40, 0x00],
    [0x00, 0x00, 0x3E, 0x40, 0x3C, 0x02, 0x7C, 0x00],
    [0x10, 0x10, 0x7C, 0x10, 0x10, 0x10, 0x0E, 0x00],
    [0x00, 0x00, 0x42, 0x42, 0x42, 0x42, 0x3E, 0x00],
    [0x00, 0x00, 0x42, 0x42, 0x42, 0x24, 0x18, 0x00],
    [0x00, 0x00, 0x42, 0x42, 0x5A, 0x5A, 0x24, 0x00],
    [0x00, 0x00, 0x42, 0x24, 0x18, 0x24, 0x42, 0x00],
    [0x00, 0x00, 0x42, 0x42, 0x42, 0x3E, 0x02, 0x3C],
    [0x00, 0x00, 0x7E, 0x04, 0x18, 0x20, 0x7E, 0x00],
];

fn glyph(c: u8) -> &'static [u8; 8] {
    match c {
        b'0'..=b'9' => &FONT_DIGITS[(c - b'0') as usize],
        b'A'..=b'Z' => &FONT_UPPER[(c - b'A') as usize],
        b'a'..=b'z' => &FONT_LOWER[(c - b'a') as usize],
        b':' => &FONT_COLON,
        b'.' => &FONT_DOT,
        b'-' => &FONT_DASH,
        b'/' => &FONT_SLASH,
        b'_' => &FONT_UNDER,
        b'*' => &FONT_STAR,
        b',' => &FONT_COMMA,
        b'(' => &FONT_LPAREN,
        b')' => &FONT_RPAREN,
        b'?' => &FONT_QUESTION,
        b'!' => &FONT_BANG,
        b'=' => &FONT_EQUALS,
        b'#' => &FONT_HASH,
        b'%' => &FONT_PERCENT,
        b'>' => &FONT_GT,
        _ => &FONT_SPACE,
    }
}

// ==================== 状态 ====================

static mut FB_PTR: *mut u32 = core::ptr::null_mut();
static mut FB_W: usize = DISPLAY_W;
static mut FB_H: usize = DISPLAY_H;
static mut FB_STRIDE_PX: usize = DISPLAY_W;
/// /dev/fb0 的描述符（呈现 ioctl 用；保持打开直至进程退出）。
static mut FB_FD: i32 = -1;
static mut BACK_BUF: [u32; DISPLAY_W * DISPLAY_H] = [0u32; DISPLAY_W * DISPLAY_H];
static mut STEP: usize = 0;
static mut LANG: usize = 0; // 0=zh_CN 1=en_US
static mut REGION: usize = 0; // 0=asia 1=europe 2=america 3=other
static mut TIMEZONE: usize = 0;
static mut NET_SKIP: bool = false;
static mut AGREEMENT: bool = false;
static mut CLOUD_ENABLED: bool = false;
static mut CLOUD_CONTROL: bool = false;
static mut CLOUD_FIND: bool = false;
static mut CLOUD_LOCK: bool = false;
static mut CLOUD_WIPE: bool = false;
static mut PARENTAL: bool = false;
static mut PARENTAL_FILTER: bool = false;
static mut PARENTAL_TIME: usize = 120;
static mut SEC_TYPE: usize = 0; // 0=pin 1=pattern 2=password
/// 是否启用了锁屏安全（用户主动选择才为 true；默认关闭 → 不写安全文件，
/// launcher 启动不自动上锁，用户可直接进入系统）。
static mut SEC_ENABLED: bool = false;
static mut DURESS_ACTION: usize = 0; // 0=fake 1=wipe 2=alert
/// 用户在「外观」步骤选择的主题（仅随用户选择写入 /data/oobe/theme 与
/// desktop_meta；系统现已固定浅色，oobe 自身 UI 也恒以浅色渲染，不再据此切色）。
static mut THEME_DARK: bool = false;
static mut WALLPAPER: usize = 2;
/// 导航方式：0=全面屏手势，1=三大虚拟按键（主题步骤选择，写入 /data/oobe/nav_mode）。
static mut NAV_MODE: usize = 0;

// ==================== 统一圆角按钮（底部导航区） ====================

/// 底部导航区按钮：返回（次要）/ 下一步（主）/ 进入系统（主）。
/// 标签与状态在 draw_nav_buttons 按步骤与语言设置；draw() 之后 x/y/w/h 即为命中区。
static mut BTN_BACK: button::Button = button::Button::new(b"", button::ButtonState::Secondary);
static mut BTN_NEXT: button::Button = button::Button::new(b"", button::ButtonState::Primary);
static mut BTN_ENTER: button::Button = button::Button::new(b"", button::ButtonState::Primary);
/// 右上角「跳过」胶囊（步骤 1..11 显示；点击放弃当前步直达完成页）。
static mut BTN_SKIP: button::Button = button::Button::new(b"", button::ButtonState::Secondary);
const SKIP_MARGIN: i32 = 16;
const SKIP_Y: i32 = 14;

// ==================== 账号 / 密码 / 胁迫密码输入框状态 ====================

/// 账号步骤：用户名缓冲；密码步骤：密码与确认；胁迫步骤：胁迫密码与确认。
/// 全部定长（<= 16 字符）。
static mut USER_BUF: [u8; 16] = [0; 16];
static mut USER_LEN: usize = 0;
static mut PASS_BUF: [u8; 16] = [0; 16];
static mut PASS_LEN: usize = 0;
static mut CONFIRM_BUF: [u8; 16] = [0; 16];
static mut CONFIRM_LEN: usize = 0;
static mut DURESS_BUF: [u8; 16] = [0; 16];
static mut DURESS_LEN: usize = 0;
static mut DURESS_CONFIRM_BUF: [u8; 16] = [0; 16];
static mut DURESS_CONFIRM_LEN: usize = 0;
/// 当前焦点输入框（按步骤解释：账号=0 用户名；密码=0 密码 / 1 确认；
/// 胁迫=0 胁迫密码 / 1 确认）。-1=无。
static mut FIELD_FOCUS: i32 = 0;

/// 当前步骤的输入框数量（账号 1 个；密码 / 胁迫各 2 个）。
fn step_field_count() -> usize {
    unsafe {
        match STEP {
            8 | 9 => 2,
            _ => 1,
        }
    }
}

/// 向焦点输入框追加一个字符（超长忽略）。
fn append_field_char(f: usize, c: u8) {
    unsafe {
        let (p, len) = field_buf(f);
        if *len < 16 {
            *p.add(*len) = c;
            *len += 1;
        }
    }
}

/// 从焦点输入框删除末尾一个字符。
fn delete_field_char(f: usize) {
    let (_, len) = field_buf(f);
    if *len > 0 {
        *len -= 1;
    }
}

/// 返回输入框的（缓冲指针，长度引用）。字段按当前步骤解释。
fn field_buf(f: usize) -> (*mut u8, &'static mut usize) {
    unsafe {
        let step = STEP;
        match (step, f) {
            (8, 1) => (CONFIRM_BUF.as_mut_ptr(), &mut CONFIRM_LEN),
            (8, _) => (PASS_BUF.as_mut_ptr(), &mut PASS_LEN),
            (9, 1) => (DURESS_CONFIRM_BUF.as_mut_ptr(), &mut DURESS_CONFIRM_LEN),
            (9, _) => (DURESS_BUF.as_mut_ptr(), &mut DURESS_LEN),
            // 账号步骤：仅用户名
            _ => (USER_BUF.as_mut_ptr(), &mut USER_LEN),
        }
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

const DEFAULT_USER: &[u8] = b"user";

const TIMEZONES: [&str; 8] = [
    "Asia/Shanghai",
    "Asia/Tokyo",
    "Asia/Seoul",
    "Europe/London",
    "Europe/Paris",
    "America/New_York",
    "America/Los_Angeles",
    "UTC",
];

const REGIONS: [&str; 4] = ["Asia", "Europe", "America", "Other"];

fn region_default_tz(region: usize) -> usize {
    match region {
        1 => 3, // 欧洲 -> 伦敦
        2 => 5, // 美洲 -> 纽约
        3 => 7, // 其他 -> UTC
        _ => 0, // 亚洲 -> 上海
    }
}

// ==================== 应用模块表 ====================

struct Module {
    key: &'static str,
    name: &'static str,
    name_zh: &'static str,
    core: bool,
    locked: bool,
    default_on: bool,
}

impl Module {
    /// 按当前语言返回显示名（默认中文）。
    fn display_name(&self) -> &'static str {
        if unsafe { LANG == 0 } { self.name_zh } else { self.name }
    }
}

static MODULES: [Module; 21] = [
    Module { key: "phone",      name: "Phone",      name_zh: "电话",      core: true,  locked: true,  default_on: true },
    Module { key: "sms",        name: "SMS",        name_zh: "短信",      core: true,  locked: true,  default_on: true },
    Module { key: "contacts",   name: "Contacts",   name_zh: "通讯录",    core: true,  locked: true,  default_on: true },
    Module { key: "settings",   name: "Settings",   name_zh: "设置",      core: true,  locked: true,  default_on: true },
    Module { key: "files",      name: "Files",      name_zh: "文件",      core: true,  locked: false, default_on: true },
    Module { key: "browser",    name: "Browser",    name_zh: "浏览器",    core: false, locked: false, default_on: true },
    Module { key: "calc",       name: "Calculator", name_zh: "计算器",    core: false, locked: false, default_on: true },
    Module { key: "calendar",   name: "Calendar",   name_zh: "日历",      core: false, locked: false, default_on: true },
    Module { key: "camera",     name: "Camera",     name_zh: "相机",      core: false, locked: false, default_on: true },
    Module { key: "clock",      name: "Clock",      name_zh: "时钟",      core: false, locked: false, default_on: true },
    Module { key: "gallery",    name: "Gallery",    name_zh: "图库",      core: false, locked: false, default_on: true },
    Module { key: "music",      name: "Music",      name_zh: "音乐",      core: false, locked: false, default_on: true },
    Module { key: "mail",       name: "Mail",       name_zh: "邮件",      core: false, locked: false, default_on: true },
    Module { key: "map",        name: "Maps",       name_zh: "地图",      core: false, locked: false, default_on: true },
    Module { key: "weather",    name: "Weather",    name_zh: "天气",      core: false, locked: false, default_on: true },
    Module { key: "store",      name: "Store",      name_zh: "应用商店",  core: false, locked: false, default_on: true },
    Module { key: "terminal",   name: "Terminal",   name_zh: "终端",      core: false, locked: false, default_on: true },
    Module { key: "cloud",      name: "Cloud",      name_zh: "云服务",    core: false, locked: false, default_on: true },
    Module { key: "video",      name: "Video",      name_zh: "视频",      core: false, locked: false, default_on: true },
    Module { key: "notes",      name: "Notes",      name_zh: "备忘录",    core: false, locked: false, default_on: true },
    // 生命周期演示/测试应用（docs/lifecycle-test.md 的 GUI 验证载体）。
    Module { key: "lcapp",      name: "LCApp",      name_zh: "LCApp",     core: false, locked: false, default_on: true },
];

static mut MODULE_ON: [bool; 21] = [false; 21];

fn module_on(i: usize) -> bool {
    unsafe { MODULE_ON[i] }
}

// ==================== 系统调用封装 ====================

#[inline(always)]
unsafe fn syscall1(n: usize, a1: usize) -> usize {
    let ret: usize;
    asm!("ecall", in("a7") n, inlateout("a0") a1 => ret, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall2(n: usize, a1: usize, a2: usize) -> usize {
    let ret: usize;
    asm!("ecall", in("a7") n, inlateout("a0") a1 => ret, in("a1") a2, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall3(n: usize, a1: usize, a2: usize, a3: usize) -> usize {
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
unsafe fn syscall6(n: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize, a6: usize) -> usize {
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

unsafe fn write_fd(fd: usize, buf: &[u8]) -> usize {
    syscall3(SYS_WRITE, fd, buf.as_ptr() as usize, buf.len())
}

fn print(s: &str) {
    unsafe { write_fd(1, s.as_bytes()); }
}

fn print_dec(mut v: usize) {
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
    unsafe { write_fd(1, &buf[i..]); }
}

/// 动画期间的输入泵：非阻塞消费输入事件，指针移动照常更新光标并重画
/// overlay（用户要求：动画没播完时指针也要可见、可动）；点击/回车动作
/// 在动画期吞掉（动画 <400ms）。
fn pump_input_during_anim() {
    let mut ev = InputEvent { x: 0, y: 0, enter: false, back: false, ch: None, backspace: false };
    while read_input_event(&mut ev) {
        // 吞掉动作字段；指针位置已在 consume_input_event 内写入 CUR_X/Y
        ev.enter = false;
        ev.back = false;
        ev.ch = None;
        ev.backspace = false;
    }
    // ★ 不设 any 门（R7-A）：read_input_event 对纯移动事件返回 false，
    // 但位置已更新、CURSOR_DIRTY 已置位——门会让动画期光标冻结
    if unsafe { CURSOR_DIRTY } {
        move_cursor_fb();
    }
}

/// 单调时钟（µs，CLOCK_MONOTONIC）：动画进度判定一律用它，帧率拉满时
/// 每帧按真实时间推进（与帧率解耦，时长恒定不随机器快慢变化）。
fn mono_us() -> u64 {
    let mut tv = [0u8; 16];
    unsafe {
        syscall3(113, 1, tv.as_mut_ptr() as usize, 0); // clock_gettime(MONOTONIC)
    }
    let sec = u64::from_le_bytes([tv[0], tv[1], tv[2], tv[3], tv[4], tv[5], tv[6], tv[7]]);
    let nsec = u64::from_le_bytes([tv[8], tv[9], tv[10], tv[11], tv[12], tv[13], tv[14], tv[15]]);
    sec * 1_000_000 + nsec / 1_000
}

fn sleep_ms(ms: usize) {
    let mut req = [0u8; 16];
    let sec = (ms / 1000) as u64;
    let nsec = ((ms % 1000) * 1_000_000) as u64;
    req[..8].copy_from_slice(&sec.to_le_bytes());
    req[8..].copy_from_slice(&nsec.to_le_bytes());
    unsafe { syscall2(SYS_NANOSLEEP, req.as_ptr() as usize, 0); }
}

unsafe fn exit(code: usize) -> ! {
    syscall1(SYS_EXIT, code);
    loop {}
}

// ==================== 文件操作 ====================

static mut PATH_BUF: [u8; 128] = [0u8; 128];

unsafe fn path_ptr(path: &str) -> usize {
    let n = path.len().min(PATH_BUF.len() - 1);
    PATH_BUF[..n].copy_from_slice(&path.as_bytes()[..n]);
    PATH_BUF[n] = 0;
    PATH_BUF.as_ptr() as usize
}

unsafe fn open(path: &str, flags: usize) -> isize {
    syscall3(SYS_OPENAT, AT_FDCWD, path_ptr(path), flags) as isize
}

unsafe fn mkdir(path: &str) -> isize {
    syscall3(SYS_MKDIRAT, AT_FDCWD, path_ptr(path), 0o755) as isize
}

/// 创建 /data 与 /data/oobe（幂等，忽略已存在的错误）。
fn mkdir_all() {
    unsafe {
        let r1 = mkdir("/data");
        let r2 = mkdir("/data/oobe");
        // -17 = EEXIST（目录已存在，正常）；其它负值才是真实失败。
        if (r1 != 0 && r1 != -17) || (r2 != 0 && r2 != -17) {
            print("[oobe] 致命错误：无法创建 /data 或 /data/oobe（r1=");
            print_dec((-r1) as usize);
            print(" r2=");
            print_dec((-r2) as usize);
            print("）\n");
            show_error("无法创建数据目录".as_bytes());
        }
    }
    print("[oobe] /data/oobe 就绪\n");
}

/// 写文件（open O_CREAT|O_TRUNC + write + close）。
fn write_file(path: &str, data: &[u8]) -> bool {
    unsafe {
        let fd = open(path, O_CREAT | O_TRUNC | O_WRONLY);
        if fd >= 0 {
            let n = write_fd(fd as usize, data);
            syscall1(SYS_CLOSE, fd as usize);
            if n != data.len() {
                print("[oobe] 写入过短：");
                print(path);
                print("\n");
                show_error("配置保存失败".as_bytes());
                return false;
            }
            true
        } else {
            print("[oobe] 打开失败：");
            print(path);
            print("\n");
            show_error("配置保存失败".as_bytes());
            false
        }
    }
}

// ==================== 帧缓冲初始化 ====================

/// 打开 /dev/fb0、查询分辨率并 mmap 到用户空间；成功返回 true。
fn fb_init() -> bool {
    unsafe {
        let fd = open(FB_PATH, O_RDWR);
        if fd < 0 {
            return false;
        }
        // 查询分辨率（24 字节：xres/yres/xres_virtual/yres_virtual/bpp/stride）
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
        // 保留 fd：呈现（FBIO_PUT_DAMAGE / FBIO_PRESENT）需要它；进程退出时回收。
        FB_FD = fd as i32;
        true
    }
}

/// 触发内核侧启动画面淡出：首帧渲染完成后调用一次；无启动画面时内核直接
/// 返回（open/ioctl 失败也静默跳过，不阻塞启动流程）。
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

/// 首帧之前的同步字体加载：先加载 OOBE 超小子集（oobe-cn/oobe-bold）再渲染
/// 首帧，向导自始至终显示平滑 TTF 汉字（无点阵闪现）。加载失败时回退内置
/// 点阵继续，不阻塞向导。
fn load_fonts_sync() {
    let ok = ttf_text::font_init_oobe();
    print("[oobe] font_init ");
    if ok {
        print("done (oobe uiputong + oobe bold)\n");
    } else {
        print("failed，回退点阵兜底\n");
    }
}

/// 呈现当前 canvas 帧：提交整屏脏区并请求合成器翻页（`flip`/转场/脉冲逐帧调用）。
fn present_fb() {
    unsafe {
        if FB_FD < 0 || FB_PTR.is_null() {
            return;
        }
        // 脏区头：[count:u32][rect{x:i32,y:i32,w:u32,h:u32}]，count=1 整屏。
        let mut hdr = [0u8; 20];
        hdr[0] = 1;
        hdr[4..8].copy_from_slice(&0i32.to_le_bytes());
        hdr[8..12].copy_from_slice(&0i32.to_le_bytes());
        hdr[12..16].copy_from_slice(&(FB_W as u32).to_le_bytes());
        hdr[16..20].copy_from_slice(&(FB_H as u32).to_le_bytes());
        syscall3(SYS_IOCTL, FB_FD as usize, FBIO_PUT_DAMAGE, hdr.as_ptr() as usize);
        // count=0 → 仅呈现（合成器把 canvas 合成到 back 并翻页）。
        let empty = [0u8; 4];
        syscall3(SYS_IOCTL, FB_FD as usize, FBIO_PRESENT, empty.as_ptr() as usize);
    }
}

/// 只呈现 [x,y,w,h] 小矩形（光标/呼吸/选中脉冲用）：脏矩形内 canvas→back
/// 增量拷贝 + 覆盖窗口合成 + 翻页，避免每帧整屏 2MB 拷贝（QEMU 下卡顿主因）。
fn present_rect(x: i32, y: i32, w: i32, h: i32) {
    unsafe {
        if FB_FD < 0 || FB_PTR.is_null() {
            return;
        }
        // 裁剪到屏幕：调用方（如光标 overlay 的旧+新位置并集）可能给出
        // 超出屏幕的矩形 —— 此前直接丢弃导致该帧更新整个丢失（光标瞬移
        // 停滞一帧），裁剪后仍呈现可见部分。
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w).min(DISPLAY_W as i32);
        let y1 = (y + h).min(DISPLAY_H as i32);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let mut hdr = [0u8; 20];
        hdr[0] = 1;
        hdr[4..8].copy_from_slice(&x0.to_le_bytes());
        hdr[8..12].copy_from_slice(&y0.to_le_bytes());
        hdr[12..16].copy_from_slice(&((x1 - x0) as u32).to_le_bytes());
        hdr[16..20].copy_from_slice(&((y1 - y0) as u32).to_le_bytes());
        syscall3(SYS_IOCTL, FB_FD as usize, FBIO_PUT_DAMAGE, hdr.as_ptr() as usize);
        let empty = [0u8; 4];
        syscall3(SYS_IOCTL, FB_FD as usize, FBIO_PRESENT, empty.as_ptr() as usize);
    }
}

/// 把 BACK_BUF 的指定区域拷到显存并脏矩形呈现（动画期替代整屏 flip：
/// 每帧只拷贝/提交变化的矩形，2MB 全屏拷贝是 TCG 下帧率的最大杀手）。
fn present_region(x0: i32, y0: i32, x1: i32, y1: i32) {
    unsafe {
        if FB_PTR.is_null() {
            return;
        }
        let cx0 = x0.clamp(0, DISPLAY_W as i32);
        let cy0 = y0.clamp(0, DISPLAY_H as i32);
        let cx1 = x1.clamp(0, DISPLAY_W as i32);
        let cy1 = y1.clamp(0, DISPLAY_H as i32);
        if cx0 >= cx1 || cy0 >= cy1 {
            return;
        }
        let w = (cx1 - cx0) as usize;
        for y in cy0..cy1 {
            core::ptr::copy_nonoverlapping(
                BACK_BUF.as_ptr().add(y as usize * DISPLAY_W + cx0 as usize),
                FB_PTR.add(y as usize * FB_STRIDE_PX + cx0 as usize),
                w,
            );
        }
        present_rect(cx0, cy0, cx1 - cx0, cy1 - cy0);
    }
}

/// 把 back buffer 一次性拷贝到显存并呈现。
///
/// 当显存 stride 与逻辑宽度一致时执行单次 `copy_nonoverlapping`（最快路径）；
/// 否则退化为逐行拷贝，兼容带 padding 的 stride。
fn flip() {
    unsafe {
        if FB_PTR.is_null() {
            return;
        }
        if FB_W == DISPLAY_W && FB_H == DISPLAY_H && FB_STRIDE_PX == DISPLAY_W {
            core::ptr::copy_nonoverlapping(
                BACK_BUF.as_ptr(),
                FB_PTR,
                DISPLAY_W * DISPLAY_H,
            );
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
    }
    present_fb();
}

// ==================== 绘制原语 ====================

fn put_pixel(x: i32, y: i32, color: u32) {
    if x < 0 || y < 0 || x >= DISPLAY_W as i32 || y >= DISPLAY_H as i32 {
        return;
    }
    unsafe { BACK_BUF[y as usize * DISPLAY_W + x as usize] = color; }
}

// ==================== 毛玻璃 / 液态玻璃（OOBE 浅色主题） ====================

use sys_nav::glass::{GlassStyle, GlassTarget, STYLE_DANGER, glass_rect, glass_rect_style};

/// OOBE 毛玻璃目标：背景取自 BACK_BUF，像素经 put_pixel 写入。
struct OobeGlass;
impl GlassTarget for OobeGlass {
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

/// 毛玻璃面板（OOBE 为浅色主题，默认浅色玻璃）。
fn draw_glass(x: i32, y: i32, w: i32, h: i32, r: i32) {
    glass_rect(&mut OobeGlass, x, y, w, h, r, false, true);
}

/// 自定义样式液态玻璃面板。
fn draw_glass_style(x: i32, y: i32, w: i32, h: i32, r: i32, st: &GlassStyle) {
    glass_rect_style(&mut OobeGlass, x, y, w, h, r, st, true);
}

/// 主按钮玻璃样式：浅色磨砂（白色着色 + 浅灰描边，液态玻璃质感）。
static STYLE_PRIMARY: GlassStyle = GlassStyle {
    tint: 0x00_FF_FF_FF,
    alpha: 225,
    spec: 60,
    sheen: 18,
    border: 0x00_D9_DE_E6,
    border_a: 220,
};

/// 次要芯片按钮玻璃样式：白色着色 + 浅灰描边。
static STYLE_SECONDARY: GlassStyle = GlassStyle {
    tint: 0x00_FF_FF_FF,
    alpha: 232,
    spec: 66,
    sheen: 26,
    border: 0x00_D9_DE_E6,
    border_a: 200,
};

/// 底部导航面板：白色半透玻璃 + 浅灰顶描边（浅色「底sheet」）。
static STYLE_SHEET: GlassStyle = GlassStyle {
    tint: 0x00_FF_FF_FF,
    alpha: 222,
    spec: 34,
    sheen: 12,
    border: 0x00_D9_DE_E6,
    border_a: 170,
};

fn fill_rect(x: i32, y: i32, w: i32, h: i32, color: u32) {
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

/// 实心圆（抗锯齿）。
fn fill_circle(cx: i32, cy: i32, r: i32, color: u32) {
    sys_nav::glass::smooth_fill_circle(&mut OobeSmooth, cx, cy, r, color);
}

/// BACK_BUF 抗锯齿绘制目标（oobe 自有帧缓冲）。
struct OobeSmooth;

impl sys_nav::glass::SmoothTarget for OobeSmooth {
    fn put_aa(&mut self, x: i32, y: i32, color: u32, alpha: u32) {
        if x < 0 || y < 0 || x >= DISPLAY_W as i32 || y >= DISPLAY_H as i32 {
            return;
        }
        unsafe {
            let idx = y as usize * DISPLAY_W + x as usize;
            BACK_BUF[idx] = blend_px(BACK_BUF[idx], color, alpha);
        }
    }
}

/// 填充圆角矩形（抗锯齿超椭圆连续曲率，ColorOS 15 观感）。
fn fill_rounded(x: i32, y: i32, w: i32, h: i32, r: i32, color: u32) {
    sys_nav::glass::smooth_fill_rr(&mut OobeSmooth, x, y, w, h, r, color);
}

// ==================== TTF 平滑字体（cn.ttf，可回退内置点阵字库） ====================

/// 码点是否可用已加载字体渲染（字体就绪且包含该字形）。
fn ttf_has(cp: u32) -> bool {
    ttf_text::font_render::font_ready() && ttf_text::font_render::has_glyph(cp)
}

/// 点阵回退的缩放系数：把 8x8 ASCII / 16x16 CJK 位图放大到贴近目标字号。
fn bitmap_scale(px: i32, native: i32) -> i32 {
    let s = (px + native - 1) / native;
    if s < 1 { 1 } else { s }
}

/// 位图回退的字形推进宽度（与绘制一致）。
fn bitmap_advance(cp: u32, px: i32) -> i32 {
    if cp < 0x80 {
        8 * bitmap_scale(px, 8)
    } else if cp <= 0xFFFF && cjk_font::cjk_index(cp as u16).is_some() {
        16 * bitmap_scale(px, 16)
    } else {
        16
    }
}

/// 把前景色按 alpha 淡出到白色背景（浅色主题背景恒为白色）。
fn fade_to_white(color: u32, alpha: u32) -> u32 {
    ttf_text::font_render::blend(C_WHITE, color, alpha as u8)
}

/// 绘制内置点阵字形（8x8 ASCII / 16x16 CJK，按字号缩放）；点阵中也没有的
/// 码点只推进宽度、不绘制任何方块。返回推进宽度。
fn draw_bitmap_scaled_a(x: i32, y: i32, cp: u32, px: i32, color: u32, alpha: u32) -> i32 {
    if alpha == 0 {
        return bitmap_advance(cp, px);
    }
    let fill = fade_to_white(color, alpha);
    if cp < 0x80 {
        let g = glyph(cp as u8);
        let s = bitmap_scale(px, 8);
        for row in 0..8 {
            for col in 0..8 {
                if g[row] & (0x80u8 >> col) != 0 {
                    fill_rect(x + col as i32 * s, y + row as i32 * s, s, s, fill);
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
                    fill_rect(x + col as i32 * s, y + row as i32 * s, s, s, fill);
                }
            }
        }
        return 16 * s;
    }
    if cp >= 0x80 { 16 } else { 8 }
}

/// 把单个码点绘制到 (x, y)（y 为字形顶部）。TTF 平滑字形优先；字体未加载
/// 或缺少该字形时回退内置点阵字库；两者都不可用时只推进宽度（不画乱码方块）。
/// `alpha`（0..255）用于半加粗的第二遍叠加。返回推进宽度。
fn ttf_glyph_a(x: i32, y: i32, cp: u32, color: u32, px: f32, alpha: u32) -> i32 {
    if ttf_has(cp) {
        let ch = core::char::from_u32(cp).unwrap_or(' ');
        if let Some(g) = ttf_text::font_render::rasterize_char(ch, px) {
            let gx = x + g.xmin;
            let gy = ttf_text::font_render::glyph_draw_top(y, g.ymin, g.height, px);
            for row in 0..g.height {
                for col in 0..g.width {
                    let v = g.data[row * g.width + col];
                    if v != 0 {
                        let px_ = gx + col as i32;
                        let py_ = gy + row as i32;
                        let in_b = px_ >= 0 && py_ >= 0 && px_ < DISPLAY_W as i32 && py_ < DISPLAY_H as i32;
                        if in_b {
                            let a = ((v as u32) * alpha) / 255;
                            let bg = unsafe { BACK_BUF[py_ as usize * DISPLAY_W + px_ as usize] };
                            put_pixel(px_, py_, ttf_text::font_render::blend(bg, color, a as u8));
                        }
                    }
                }
            }
            return g.advance;
        }
    }
    draw_bitmap_scaled_a(x, y, cp, px as i32, color, alpha)
}

fn ttf_glyph(x: i32, y: i32, cp: u32, color: u32, px: f32) -> i32 {
    ttf_glyph_a(x, y, cp, color, px, 255)
}

/// 单个码点的推进宽度（与 ttf_glyph_a 的绘制宽度一致）。
fn glyph_advance(cp: u32, px: f32) -> i32 {
    if ttf_has(cp) {
        return ttf_text::font_render::advance(cp, px);
    }
    bitmap_advance(cp, px as i32)
}

/// 以标题字体（bold.ttf）绘制单个码点（带 alpha）。标题字体就绪时用真实
/// 粗体字形；否则退回常规字体 + 模拟加粗（两遍叠加）。
fn ttf_glyph_bold_a(x: i32, y: i32, cp: u32, color: u32, px: f32, alpha: u32) -> i32 {
    if ttf_text::font_render::has_glyph_bold(cp) {
        let ch = core::char::from_u32(cp).unwrap_or(' ');
        if let Some(g) = ttf_text::font_render::rasterize_char_bold(ch, px) {
            let gx = x + g.xmin;
            let gy = ttf_text::font_render::glyph_draw_top_bold(y, g.ymin, g.height, px);
            for row in 0..g.height {
                for col in 0..g.width {
                    let v = g.data[row * g.width + col];
                    if v != 0 {
                        let px_ = gx + col as i32;
                        let py_ = gy + row as i32;
                        let in_b =
                            px_ >= 0 && py_ >= 0 && px_ < DISPLAY_W as i32 && py_ < DISPLAY_H as i32;
                        if in_b {
                            let a = ((v as u32) * (alpha % 256)) / 255;
                            let bg = unsafe { BACK_BUF[py_ as usize * DISPLAY_W + px_ as usize] };
                            put_pixel(px_, py_, ttf_text::font_render::blend(bg, color, a as u8));
                        }
                    }
                }
            }
            return g.advance;
        }
    }
    ttf_glyph_a(x, y, cp, color, px, alpha)
}

/// 标题字体的字形推进宽度（不可用时退回常规宽度，保证与绘制一致）。
fn glyph_advance_bold(cp: u32, px: f32) -> i32 {
    if ttf_text::font_render::has_glyph_bold(cp) {
        return ttf_text::font_render::advance_bold(cp, px);
    }
    glyph_advance(cp, px)
}

/// 按 UTF-8 渲染文本（px 字号）。
fn draw_text_px(x: i32, y: i32, s: &[u8], px: f32, color: u32) {
    let mut cx = x;
    let mut i = 0;
    while i < s.len() {
        let (cp, n) = ttf_text::font_render::utf8_next(s, i);
        cx += ttf_glyph_a(cx, y, cp, color, px, 255);
        i += n;
    }
}

/// 标题文本（真粗体，bold.ttf）：标题字体就绪时用其实字体渲染，否则退回
/// 常规字体两遍叠加模拟加粗。
fn draw_text_bold_px(x: i32, y: i32, s: &[u8], px: f32, color: u32) {
    let mut cx = x;
    let mut i = 0;
    while i < s.len() {
        let (cp, n) = ttf_text::font_render::utf8_next(s, i);
        if ttf_text::font_render::font_bold_ready() && ttf_text::font_render::has_glyph_bold(cp) {
            cx += ttf_glyph_bold_a(cx, y, cp, color, px, 255);
        } else {
            ttf_glyph_a(cx, y, cp, color, px, 255);
            ttf_glyph_a(cx + 1, y, cp, color, px, 255);
            cx += glyph_advance(cp, px);
        }
        i += n;
    }
}

/// 半加粗（17px 按钮等）：标题字体就绪时直接使用粗体字形，否则第二遍以
/// 128 alpha 叠加模拟。
fn draw_text_semibold_px(x: i32, y: i32, s: &[u8], px: f32, color: u32) {
    let mut cx = x;
    let mut i = 0;
    while i < s.len() {
        let (cp, n) = ttf_text::font_render::utf8_next(s, i);
        if ttf_text::font_render::font_bold_ready() && ttf_text::font_render::has_glyph_bold(cp) {
            cx += ttf_glyph_bold_a(cx, y, cp, color, px, 255);
        } else {
            ttf_glyph_a(cx, y, cp, color, px, 255);
            ttf_glyph_a(cx + 1, y, cp, color, px, 128);
            cx += glyph_advance(cp, px);
        }
        i += n;
    }
}

fn text_width_px(s: &[u8], px: f32) -> i32 {
    let mut w = 0i32;
    let mut i = 0;
    while i < s.len() {
        let (cp, n) = ttf_text::font_render::utf8_next(s, i);
        w += glyph_advance(cp, px);
        i += n;
    }
    w
}

/// 标题字体下的文本宽度（对齐 draw_text_bold_px 的真实推进）。
fn text_width_px_bold(s: &[u8], px: f32) -> i32 {
    let mut w = 0i32;
    let mut i = 0;
    while i < s.len() {
        let (cp, n) = ttf_text::font_render::utf8_next(s, i);
        w += glyph_advance_bold(cp, px);
        i += n;
    }
    w
}

/// 水平居中文本（px 字号）。
fn draw_center_px(y: i32, s: &[u8], px: f32, color: u32) {
    let tx = (DISPLAY_W as i32 - text_width_px(s, px)) / 2;
    draw_text_px(tx, y, s, px, color);
}

/// 水平居中加粗文本。
fn draw_center_bold_px(y: i32, s: &[u8], px: f32, color: u32) {
    let tx = (DISPLAY_W as i32 - text_width_px_bold(s, px)) / 2;
    draw_text_bold_px(tx, y, s, px, color);
}

/// 带 alpha（淡入动画）的文本绘制：每个字形按 alpha 透明度混合到背景。
fn draw_text_px_a(x: i32, y: i32, s: &[u8], px: f32, color: u32, alpha: u32) {
    let mut cx = x;
    let mut i = 0;
    while i < s.len() {
        let (cp, n) = ttf_text::font_render::utf8_next(s, i);
        cx += ttf_glyph_a(cx, y, cp, color, px, (alpha % 256) as u32);
        i += n;
    }
}

/// 带 alpha 的标题绘制（标题字体就绪时用真实粗体，否则两遍叠加模拟）。
fn draw_text_bold_px_a(x: i32, y: i32, s: &[u8], px: f32, color: u32, alpha: u32) {
    let mut cx = x;
    let mut i = 0;
    while i < s.len() {
        let (cp, n) = ttf_text::font_render::utf8_next(s, i);
        if ttf_text::font_render::font_bold_ready() && ttf_text::font_render::has_glyph_bold(cp) {
            cx += ttf_glyph_bold_a(cx, y, cp, color, px, alpha);
        } else {
            ttf_glyph_a(cx, y, cp, color, px, (alpha % 256) as u32);
            ttf_glyph_a(cx + 1, y, cp, color, px, ((alpha.min(128) * 2 + alpha) / 3).min(255));
            cx += glyph_advance(cp, px);
        }
        i += n;
    }
}

/// 带 alpha 的水平居中文本。
fn draw_center_px_a(y: i32, s: &[u8], px: f32, color: u32, alpha: u32) {
    let tx = (DISPLAY_W as i32 - text_width_px(s, px)) / 2;
    draw_text_px_a(tx, y, s, px, color, alpha);
}

/// 带 alpha 的水平居中加粗文本。
fn draw_center_bold_px_a(y: i32, s: &[u8], px: f32, color: u32, alpha: u32) {
    let tx = (DISPLAY_W as i32 - text_width_px_bold(s, px)) / 2;
    draw_text_bold_px_a(tx, y, s, px, color, alpha);
}

/// 兼容入口：16px 常规文本（finish 过渡 UI 使用）。
fn draw_text(x: i32, y: i32, s: &[u8], color: u32) {
    draw_text_px(x, y, s, ttf_text::font_render::FONT_PX, color);
}

/// 兼容入口：16px 宽度。
fn text_width(s: &[u8]) -> i32 {
    text_width_px(s, ttf_text::font_render::FONT_PX)
}

fn draw_center(y: i32, s: &[u8], color: u32) {
    draw_center_px(y, s, ttf_text::font_render::FONT_PX, color);
}

fn draw_char(x: i32, y: i32, c: u8, color: u32) {
    ttf_glyph(x, y, c as u32, color, ttf_text::font_render::FONT_PX);
}

fn draw_cjk(x: i32, y: i32, cp: u16, color: u32) {
    ttf_glyph(x, y, cp as u32, color, ttf_text::font_render::FONT_PX);
}

/// ColorOS 15 浅色向导背景：近白底 + 极淡绿冷色垂直渐变。
/// 背景渐变**行缓存**：背景静态，每行颜色只算一次，之后每帧整行 u32
/// 拷贝（动画期 draw_background 每帧调用，原版每帧重算 47 万次渐变混合）。
static mut BG_ROWS: [u32; DISPLAY_H] = [0; DISPLAY_H];
static mut BG_ROWS_READY: bool = false;

fn draw_background() {
    unsafe {
        if !BG_ROWS_READY {
            let (tr, tg, tb) = (0xF9u32, 0xFBu32, 0xFD);
            let (br, bg, bb) = (0xE9u32, 0xF1u32, 0xF4);
            for y in 0..DISPLAY_H as i32 {
                let t = (y as u32 * 255 / DISPLAY_H as u32).min(255);
                let inv = 255 - t;
                let r = (tr * inv + br * t) / 255;
                let g = (tg * inv + bg * t) / 255;
                let b = (tb * inv + bb * t) / 255;
                BG_ROWS[y as usize] = (r << 16) | (g << 8) | b;
            }
            BG_ROWS_READY = true;
        }
        for y in 0..DISPLAY_H as i32 {
            let row = BG_ROWS[y as usize];
            let iy = y as usize * DISPLAY_W;
            BACK_BUF[iy..iy + DISPLAY_W].fill(row);
        }
    }
}

/// 标准 alpha 混合（本地实现，供动画逐帧合成）。
fn blend_px(a: u32, b: u32, t: u32) -> u32 {
    let t = t.min(255);
    let inv = 255 - t;
    let r = (((a >> 16) & 0xFF) * inv + ((b >> 16) & 0xFF) * t) / 255;
    let g = (((a >> 8) & 0xFF) * inv + ((b >> 8) & 0xFF) * t) / 255;
    let bl = ((a & 0xFF) * inv + (b & 0xFF) * t) / 255;
    (r << 16) | (g << 8) | bl
}

/// 二次平滑插值（对标 EaseInEaseOut 的近似曲线，动画进度 x ∈ [0,1]）。
fn ease_step(t: f32) -> f32 {
    if t <= 0.0 {
        0.0
    } else if t >= 1.0 {
        1.0
    } else {
        t * t * (3.0 - 2.0 * t)
    }
}

/// 快速出场（元素入场用）：1 - (1-t)^2。
fn ease_out(t: f32) -> f32 {
    if t <= 0.0 {
        0.0
    } else if t >= 1.0 {
        1.0
    } else {
        1.0 - (1.0 - t) * (1.0 - t)
    }
}

/// ColorOS 弹跳过冲（back ease，对标 ColorOS 15 徽标/页面入场）：
/// 1.0 + c3·(t-1)³ + c1·(t-1)²，进度末端轻微过冲后回落到 1.0。
fn ease_out_back(t: f32) -> f32 {
    if t <= 0.0 {
        0.0
    } else if t >= 1.0 {
        1.0
    } else {
        let c1 = 1.70158;
        let c3 = c1 + 1.0;
        1.0 + c3 * (t - 1.0) * (t - 1.0) * (t - 1.0) + c1 * (t - 1.0) * (t - 1.0)
    }
}

/// Bresenham 直线（像素级，供复选勾/返回箭头等几何绘制）。
fn draw_line(x0: i32, y0: i32, x1: i32, y1: i32, color: u32) {
    let mut x = x0;
    let mut y = y0;
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        put_pixel(x, y, color);
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

/// 2px 加粗直线（复选勾等）。
fn draw_line_thick(x0: i32, y0: i32, x1: i32, y1: i32, color: u32) {
    draw_line(x0, y0, x1, y1, color);
    draw_line(x0, y0 + 1, x1, y1 + 1, color);
}

/// 圆角矩形 1px 描边（抗锯齿：直边锐利、圆角段随曲线淡出）。
fn card_border(x: i32, y: i32, w: i32, h: i32, r: i32, color: u32) {
    sys_nav::glass::smooth_stroke_rr(&mut OobeSmooth, x, y, w, h, r, color, 255);
}

/// ColorOS 15 白色选择卡片：未选中 = 白底 + 中性灰描边；选中 = 淡绿底 + 绿描边。
fn draw_option_card(x: i32, y: i32, w: i32, h: i32, selected: bool) {
    if selected {
        fill_rounded(x, y, w, h, CARD_R, C_ACCENT_TINT);
        card_border(x, y, w, h, CARD_R, C_ACCENT_STRONG);
    } else {
        fill_rounded(x, y, w, h, CARD_R, C_WHITE);
        card_border(x, y, w, h, CARD_R, C_CARD_BORDER);
    }
}

/// ColorOS 15 单选圈（24x24）：未选中灰圈，选中 绿圈 + 白芯。
/// `color` 参数保留兼容旧调用方（选中色恒为ColorOS 绿）。
fn draw_radio(x: i32, y: i32, selected: bool, _color: u32) {
    let cx = x + 12;
    let cy = y + 12;
    for dy in -12..=12 {
        for dx in -12..=12 {
            let d = dx * dx + dy * dy;
            if d <= 121 && d >= 64 {
                put_pixel(cx + dx, cy + dy, if selected { C_ACCENT_STRONG } else { C_RADIO_BORDER });
            }
        }
    }
    if selected {
        fill_circle(cx, cy, 6, C_ACCENT);
        put_pixel(cx, cy, C_WHITE);
    }
}

/// ColorOS 15 复选方框（24x24 圆角）：未选中灰边，选中 绿填充 + 白色对勾。
/// `color` 参数保留兼容旧调用方。
fn draw_checkbox(x: i32, y: i32, checked: bool, _color: u32) {
    let r = 6;
    if checked {
        fill_rounded(x, y, 24, 24, r, C_ACCENT);
        draw_line_thick(x + 6, y + 13, x + 10, y + 17, C_WHITE);
        draw_line_thick(x + 10, y + 17, x + 18, y + 7, C_WHITE);
    } else {
        card_border(x, y, 24, 24, r, C_RADIO_BORDER);
    }
}

/// 欢迎页 Hero：渐变绿圆盘 + 白色 K 字（`e` ∈ [0,1] 为入场进度，
/// 叠加透明淡入与缩放；ColorOS 15 风格徽标）。
fn draw_brand(e: f32) {
    draw_brand_ex(e, 0.72 + 0.28 * e);
}

/// 闲置呼吸：Hero 持续微缩放（0.975..1.025 正弦三角振荡），ColorOS 常驻动效。
fn draw_brand_idle(sc: f32) {
    draw_brand_ex(1.0, sc);
}

fn draw_brand_ex(e: f32, scale: f32) {
    let r = (HERO_R as f32 * scale) as i32;
    let a = (255.0 * e) as u32;
    let cy = HERO_CY + ((1.0 - e) * 10.0) as i32;
    let glow_r = r + 10;
    // ColorOS 入场光晕：徽标背后先扩散一圈淡绿柔光（淡入淡出）。
    // scanline 优化（帧率关键）：每行先解出圆内水平段 [x0,x1]，段内
    // d 只随 dx 线性变化，免去逐像素圆判定与越界 continue（TCG 实测
    // 该圆双环逐像素版 ~13ms/帧，是动画帧率第一大杀手）。
    if (a as f32) > 0.0 {
        for dy in -glow_r..=glow_r {
            let y = cy + dy;
            if y < 0 || y >= DISPLAY_H as i32 {
                continue;
            }
            let span2 = glow_r * glow_r - dy * dy;
            if span2 <= 0 {
                continue;
            }
            let half = isqrt(span2) ;
            let x0 = (HERO_CX - half).max(0);
            let x1 = (HERO_CX + half).min(DISPLAY_W as i32 - 1);
            let mut dx = x0 - HERO_CX;
            let mut x = x0;
            while x <= x1 {
                let d = dx * dx + dy * dy;
                let fall = (glow_r * glow_r - d) * (a as i32) / (glow_r * glow_r);
                let ga = (fall / 8).min(120) as u32;
                if ga > 0 {
                    let g = blend_px(put_pixel_get(x, y), 0x00_5A_D2_8A, ga);
                    put_pixel(x, y, g);
                }
                dx += 1;
                x += 1;
            }
        }
    }
    for dy in -r..=r {
        let y = cy + dy;
        if y < 0 || y >= DISPLAY_H as i32 {
            continue;
        }
        let span2 = r * r - dy * dy;
        if span2 <= 0 {
            continue;
        }
        let half = isqrt(span2) ;
        // ColorOS 绿垂直渐变（顶部亮绿 → 底部深绿）：行内同色，一次算好
        let t = ((dy + r) * 255 / (2 * r)).min(255) as u32;
        let c = blend_px(0x00_45_E0_8B, 0x00_09_9E_58, t);
        let disc = blend_px(0x00_F0_FA_F6, c, a);
        let x0 = (HERO_CX - half).max(0);
        let x1 = (HERO_CX + half).min(DISPLAY_W as i32 - 1);
        let yl = y as usize * DISPLAY_W;
        for x in x0..=x1 {
            unsafe { BACK_BUF[yl + x as usize] = disc };
        }
    }
    // 内圈高光（ColorOS 徽标镜面感）。
    for dy in -12..=12 {
        for dx in -12..=12 {
            if dx * dx + dy * dy <= 144 {
                put_pixel(HERO_CX + dx, cy - 8 + dy, blend_px(
                    put_pixel_get(HERO_CX + dx, cy - 8 + dy),
                    0x00_FF_FF_FF,
                    ((10 - dy.max(1)) * 14).clamp(0, 60) as u32,
                ));
            }
        }
    }
    // K 字（白色，scaled 由调用方用大字号文本叠加）。
    draw_center_bold_px(cy - 16, "K".as_bytes(), 40.0, 0x00_FF_FF_FF);
}

/// 读取背景像素（供 Hero 高光混合）。
fn put_pixel_get(x: i32, y: i32) -> u32 {
    if x < 0 || y < 0 || x >= DISPLAY_W as i32 || y >= DISPLAY_H as i32 {
        0
    } else {
        unsafe { BACK_BUF[y as usize * DISPLAY_W + x as usize] }
    }
}

/// 选项胶囊：选中 = 浅色磨砂主玻璃；未选中 = 白色玻璃 + 浅灰描边（液态玻璃）。
fn draw_option_pill(x: i32, y: i32, w: i32, h: i32, r: i32, selected: bool) {
    if selected {
        draw_glass_style(x, y, w, h, r, &STYLE_PRIMARY);
    } else {
        draw_glass_style(x, y, w, h, r, &STYLE_SECONDARY);
    }
}

/// 底部导航区按钮统一绘制（ColorOS 15 风格）：
///   - 欢迎页（STEP=0）：通栏「开始使用」主按钮（浅色磨砂玻璃 + 近黑文字）；
///   - 其余步骤：左下次要「返回」芯片 + 右下「下一步/进入系统」主按钮。
/// 全部通过 button 组件（圆角、固定高度、按压缩放反馈）。
fn draw_nav_buttons() {
    let mut cv = OobeCanvas;
    let by = NAV_Y + (NAV_H - button::BTN_H) / 2;
    let sw = DISPLAY_W as i32;
    unsafe {
        if STEP == 0 {
            BTN_NEXT.label = if LANG == 0 { "开始使用".as_bytes() } else { "Start".as_bytes() };
            BTN_NEXT.state = button::ButtonState::Primary;
            BTN_NEXT.draw_full(&mut cv, NAV_BTN_MARGIN, by, sw);
        } else {
            BTN_BACK.label = if LANG == 0 { "上一步".as_bytes() } else { "Back".as_bytes() };
            BTN_BACK.state = button::ButtonState::Secondary;
            BTN_BACK.draw(&mut cv, NAV_BTN_MARGIN, by, sw);
            if STEP < 12 {
                BTN_NEXT.label = if LANG == 0 { "下一步 >".as_bytes() } else { "Next >".as_bytes() };
                BTN_NEXT.state = button::ButtonState::Primary;
                let w = BTN_NEXT.width(&cv, sw);
                BTN_NEXT.draw(&mut cv, sw - NAV_BTN_MARGIN - w, by, sw);
            } else {
                BTN_ENTER.label = if LANG == 0 { "进入系统".as_bytes() } else { "Enter".as_bytes() };
                BTN_ENTER.state = button::ButtonState::Primary;
                let w = BTN_ENTER.width(&cv, sw);
                BTN_ENTER.draw(&mut cv, sw - NAV_BTN_MARGIN - w, by, sw);
            }
        }
    }
}

/// 按下反馈：光标落在可点击按钮上时置为按下态并整屏重绘。
fn btn_on_press(x: i32, y: i32) {
    let mut any = false;
    unsafe {
        for b in [&mut BTN_BACK, &mut BTN_NEXT, &mut BTN_ENTER, &mut BTN_SKIP] {
            if b.enabled() && b.hit(x, y) {
                b.press();
                any = true;
            }
        }
    }
    if any {
        render_and_flip();
    }
}

/// 松开反馈：被按下的按钮进入 150ms 回弹过渡并整屏重绘。
fn btn_on_release() {
    let mut any = false;
    unsafe {
        for b in [&mut BTN_BACK, &mut BTN_NEXT, &mut BTN_ENTER, &mut BTN_SKIP] {
            if b.pressed() {
                b.release();
                any = true;
            }
        }
    }
    if any {
        render_and_flip();
    }
}

/// 是否有按钮正处于松开回弹过渡中（主循环据此逐帧推进动画）。
fn btn_animating() -> bool {
    unsafe { BTN_BACK.animating() || BTN_NEXT.animating() || BTN_ENTER.animating() || BTN_SKIP.animating() }
}

/// 逐帧推进所有按钮的回弹过渡。
fn btn_tick_all() {
    unsafe {
        BTN_BACK.tick();
        BTN_NEXT.tick();
        BTN_ENTER.tick();
        BTN_SKIP.tick();
    }
}

/// 复位所有按钮（切步骤时调用，避免上一屏的按压/回弹状态残留）。
fn btn_reset_all() {
    unsafe {
        BTN_BACK.reset();
        BTN_NEXT.reset();
        BTN_ENTER.reset();
        BTN_SKIP.reset();
    }
}

/// 右上角「跳过」胶囊（步骤 1..11 显示；点击放弃当前步直达完成页）。
fn draw_skip_button() {
    let mut cv = OobeCanvas;
    let sw = DISPLAY_W as i32;
    unsafe {
        if (1..=11).contains(&STEP) {
            BTN_SKIP.label = if LANG == 0 { "跳过".as_bytes() } else { "Skip".as_bytes() };
            BTN_SKIP.state = button::ButtonState::Secondary;
            let w = BTN_SKIP.width(&cv, sw);
            BTN_SKIP.draw(&mut cv, sw - SKIP_MARGIN - w, SKIP_Y, sw);
        }
    }
}

/// 跳过按钮是否命中（组件命中区；未显示的步骤 rect 为空、必然不命中）。
fn btn_skip_hit(x: i32, y: i32) -> bool {
    unsafe { (1..=11).contains(&STEP) && BTN_SKIP.enabled() && BTN_SKIP.hit(x, y) }
}

/// 返回按钮是否命中（组件命中区，未绘制过则 rect 为空、必然不命中）。
fn btn_back_hit(x: i32, y: i32) -> bool {
    unsafe { STEP > 0 && BTN_BACK.enabled() && BTN_BACK.hit(x, y) }
}

/// 底部进度指示器：13 个圆点（ColorOS 15 风格：当前步骤绿，已完成淡绿，
/// 未到灰色），位于导航面板顶部一行，避开底部按钮。
fn draw_progress() {
    let total = 13;
    let gap = 18;
    let total_w = (total - 1) * gap;
    let start = (DISPLAY_W as i32 - total_w) / 2;
    let cy = NAV_Y + 10;
    for i in 0..total {
        let cx = start + i * gap;
        let step = unsafe { i as usize == STEP };
        if step {
            fill_circle(cx, cy, 5, C_ACCENT);
        } else if unsafe { (i as usize) < STEP } {
            fill_circle(cx, cy, 3, C_ACCENT_SOFT);
        } else {
            fill_circle(cx, cy, 3, C_RADIO_BORDER);
        }
    }
}

/// 转场期间的进度点动画：当前点由小放大、已推进点亮。（`e` ∈ [0,1]）
/// 放大走 back-out（末端轻微过冲后回落，§4 打磨：进度点弹性过冲）。
fn draw_progress_anim(to: usize, e: f32) {
    let total = 13;
    let gap = 18;
    let total_w = (total - 1) * gap;
    let start = (DISPLAY_W as i32 - total_w) / 2;
    let cy = NAV_Y + 10;
    // back-out：1 + c3·(t-1)³ + c1·(t-1)²，c1=1.70158、c3=c1+1
    let t = e - 1.0;
    let back = 1.0 + 2.70158 * t * t * t + 1.70158 * t * t;
    let grow = (3.0 * back).max(0.0) as i32;
    for i in 0..total {
        let cx = start + i * gap;
        if i as usize == to {
            fill_circle(cx, cy, 3 + grow, C_ACCENT);
        } else if (i as usize) < to {
            fill_circle(cx, cy, 3, C_ACCENT_SOFT);
        } else {
            fill_circle(cx, cy, 3, C_RADIO_BORDER);
        }
    }
}

fn draw_nav() {
    // ColorOS 15 底部轻玻璃面板（白色半透 + 中性灰描边）。
    draw_glass_style(0, NAV_Y, DISPLAY_W as i32, NAV_H, 0, &STYLE_SHEET);
    // 返回 / 下一步 / 进入系统：统一圆角按钮组件。
    draw_nav_buttons();
    draw_progress();
    // 错误提示横幅画在最上层（无墙钟，按帧计数自动消失）。
    draw_error_banner();
}

/// 设置一条用户可见的错误提示（OOBE 无墙钟：按帧计数，约 250 帧 ≈ 4 秒后消失）。
fn show_error(msg: &[u8]) {
    err_ui::err_set_ticks(msg, 250);
}

/// 若存在未过期的错误提示，在屏幕顶部绘制横幅（不遮挡页面标题）。
/// 红色液态玻璃材质。
fn draw_error_banner() {
    if !err_ui::err_active(0) {
        return;
    }
    let msg = err_ui::err_msg();
    if msg.is_empty() {
        return;
    }
    draw_glass_style(0, 0, DISPLAY_W as i32, 44, 0, &STYLE_DANGER);
    let tx = (DISPLAY_W as i32 - text_width_px(msg, FONT_BODY_PX)) / 2;
    draw_text_px(tx, 10, msg, FONT_BODY_PX, 0x00_FF_FF_FF);
}

// ==================== 步骤图标（澎湃 OS 3 风格：大号矢量字形 + 光锥扫光） ====================

/// 图标主色：深青蓝垂直渐变（无底座，澎湃 3 OOBE 观感；比参考亮青更深）。
const ICON_TOP: u32 = 0x00_2A_AE_DC;
const ICON_BOTTOM: u32 = 0x00_0A_6E_B4;
/// 字形栅格化字号（Phosphor em px；墨迹约 90px）。
const ICON_GLYPH_PX: f32 = 124.0;
/// 光锥高光带半宽（对角坐标 s = x+y 上的 px）。
const CONE_BAND: i32 = 46;
/// 光锥动效恢复区域半宽（≥ 字形 bbox 半宽 + 裕量）。
const CONE_AREA_HALF: i32 = 76;

/// 已播放过光锥动效的步骤（同页重绘不重播，只画终态）。
static mut CONE_STEP: i32 = -1;
/// 待播光锥动效的步骤（页面渲染提交后由 play_pending_cone 消费）。
static mut PENDING_CONE: i32 = -1;

/// 步骤 → Phosphor-Bold 图标码点（shared/icons.rs；与设置行图标同引擎同风格）。
fn step_icon_cp(step: usize) -> u32 {
    use ttf_text::font_render::icons::settings as si;
    use ttf_text::font_render::icons::ui as ui_ic;
    match step {
        1 => ui_ic::GLOBE,      // 语言：地球
        2 => si::APPS,          // 应用：四方格
        3 => ui_ic::CLOCK,      // 时区：时钟
        4 => si::WIFI,          // 网络：Wi-Fi
        5 => ui_ic::FILE,       // 协议：文档
        6 => ui_ic::USER,       // 账号：人像
        7 => ui_ic::LOCK,       // 锁屏方式：挂锁
        8 => ui_ic::KEY,        // 密码：钥匙
        9 => si::VPN,           // 胁迫密码：盾牌
        10 => ui_ic::LIGHTNING, // 胁迫动作：闪电
        11 => ui_ic::MOON,      // 外观：月牙
        _ => ui_ic::CHECK,      // 兜底：对勾
    }
}

/// 当前步骤图标：白底上大号深青蓝矢量字形（Phosphor-Bold，笔画加粗）。
/// 首次进入该步骤时只登记待播动效（PENDING_CONE），扫光由
/// play_pending_cone 在页面渲染提交后播放——扫光的同时页面已在屏上。
fn draw_step_icon(step: usize) {
    let cx = DISPLAY_W as i32 / 2;
    let cy = ICON_Y + ICON_SIZE / 2;
    let cp = step_icon_cp(step);
    unsafe {
        if CONE_STEP != step as i32 {
            PENDING_CONE = step as i32;
        }
    }
    restore_icon_bg();
    if !draw_icon_gradient(cx, cy, cp, None) {
        draw_icon_fallback(cx, cy);
    }
}

/// 页面渲染提交后调用：有待播光锥动效则播放（页面此刻已完整可见）。
fn play_pending_cone() {
    let st = unsafe { PENDING_CONE };
    if st < 0 {
        return;
    }
    unsafe {
        PENDING_CONE = -1;
        CONE_STEP = st;
    }
    if !ttf_text::font_render::icon_ready(false) {
        return; // 图标已由 draw_step_icon 画过兜底
    }
    let cp = step_icon_cp(st as usize);
    play_icon_cone(cp);
}

/// 兜底：深青蓝实心圆 + 白芯（图标字体未就绪时）。
fn draw_icon_fallback(cx: i32, cy: i32) {
    fill_circle(cx, cy, 30, ICON_BOTTOM);
    fill_circle(cx, cy, 11, C_WHITE);
}

/// 恢复图标区域背景（与 draw_background 同一渐变公式，仅填矩形）。
fn restore_icon_bg() {
    let cx = DISPLAY_W as i32 / 2;
    let cy = ICON_Y + ICON_SIZE / 2;
    let x0 = (cx - CONE_AREA_HALF).max(0);
    let x1 = (cx + CONE_AREA_HALF).min(DISPLAY_W as i32 - 1);
    let y0 = (cy - CONE_AREA_HALF).max(0);
    // 下缘钳到标题行上方：restore 在整页（含标题）已入 BACK_BUF 后每帧
    // 运行，不钳会擦掉标题最上几行墨迹并随终态 flip 一直驻留。
    let y1 = (cy + CONE_AREA_HALF)
        .min(PAGE_TITLE_Y - 1)
        .min(DISPLAY_H as i32 - 1);
    for y in y0..=y1 {
        let t = (y as u32 * 255 / DISPLAY_H as u32).min(255);
        let inv = 255 - t;
        let r = (0xF9u32 * inv + 0xE9u32 * t) / 255;
        let g = (0xFBu32 * inv + 0xF1u32 * t) / 255;
        let b = (0xFDu32 * inv + 0xF4u32 * t) / 255;
        let row = (r << 16) | (g << 8) | b;
        for x in x0..=x1 {
            unsafe { BACK_BUF[y as usize * DISPLAY_W + x as usize] = row };
        }
    }
}

/// 光锥扫光：高光带沿对角方向（左上 → 右下）扫过字形一次，~550ms。
/// 每帧只重绘图标区域背景 + 字形（渐变 + 高光带），整屏 flip 呈现。
fn play_icon_cone(cp: u32) {
    let cx = DISPLAY_W as i32 / 2;
    let cy = ICON_Y + ICON_SIZE / 2;
    // 字形整段动效只栅格化一次：ICON_GLYPH_PX 级加粗字形位图 ~17KB，远超
    // 1KB 字形缓存上限，逐帧栅格化等于每帧一次全量栅格化 + 膨胀 pass。
    // 栅格化失败（字体未就绪等）直接返回 —— 静态图标已由 draw_step_icon 画过。
    let Some(g) = ttf_text::font_render::rasterize_icon_thick(false, cp, ICON_GLYPH_PX) else {
        return;
    };
    // 真实时间推进（帧率拉满，时长恒定）：光锥扫描 ~500ms
    const CONE_DUR_US: u64 = 500_000;
    let span = 2 * CONE_AREA_HALF; // 对角坐标 s=gx+gy 在区域内的变化范围
    let base = cx + cy;
    let t0 = mono_us();
    loop {
        let p = ((mono_us().saturating_sub(t0)) as f32 / CONE_DUR_US as f32).min(1.0);
        // 光带中心从区域左上外扫到右下外。
        let s = base - span - CONE_BAND
            + ((2.0 * (span + CONE_BAND) as f32) * p) as i32;
        // 高光强度：前 55% 渐满、后 45% 淡出（光锥离去）。
        let k = if p < 0.55 { p / 0.55 } else { (1.0 - (p - 0.55) / 0.45).max(0.0) };
        let fade = (k * 255.0) as u32;
        restore_icon_bg();
        blit_icon_gradient(cx, cy, &g, Some((s, fade)));
        flip();
        pump_input_during_anim();
        if p >= 1.0 {
            break;
        }
    }
    // 终态：无高光的纯渐变字形。
    restore_icon_bg();
    blit_icon_gradient(cx, cy, &g, None);
    flip();
}

/// Phosphor-Bold 图标字形（笔画加粗）按墨迹包围盒居中绘制到 (cx, cy)，
/// 填充为深青蓝垂直渐变；`cone` 提供光锥高光带（对角中心 s、强度 0..=255）。
/// 灰度 alpha 与背景混合。字形不可用返回 false（调用方画兜底）。
fn draw_icon_gradient(cx: i32, cy: i32, cp: u32, cone: Option<(i32, u32)>) -> bool {
    if !ttf_text::font_render::icon_ready(false) {
        return false;
    }
    let Some(g) = ttf_text::font_render::rasterize_icon_thick(false, cp, ICON_GLYPH_PX) else {
        return false;
    };
    blit_icon_gradient(cx, cy, &g, cone)
}

/// 把已栅格化的加粗字形按深青蓝垂直渐变（+可选光锥高光带）绘制到
/// BACK_BUF。字形借用共享栅格缓冲 —— 调用期间不得再触发栅格化。
fn blit_icon_gradient(
    cx: i32,
    cy: i32,
    g: &ttf_text::font_render::ThickGlyph,
    cone: Option<(i32, u32)>,
) -> bool {
    if g.width == 0 || g.height == 0 {
        return false;
    }
    let x0 = cx - g.width as i32 / 2;
    let y0 = cy - g.height as i32 / 2;
    let hmax = (g.height as u32 - 1).max(1);
    for row in 0..g.height {
        let t = row as u32 * 255 / hmax;
        let base = blend_px(ICON_TOP, ICON_BOTTOM, t);
        for col in 0..g.width {
            let a = g.data[row * g.width + col] as u32;
            if a == 0 {
                continue;
            }
            let gx = x0 + col as i32;
            let gy = y0 + row as i32;
            if gx < 0 || gy < 0 || gx >= DISPLAY_W as i32 || gy >= DISPLAY_H as i32 {
                continue;
            }
            let mut c = base;
            if let Some((s, fade)) = cone {
                if fade > 0 {
                    let d = (gx + gy - s).abs();
                    if d < CONE_BAND {
                        // 带内白光：三角形衰减 × 动效包络，峰值约 55% 白。
                        let kk = ((CONE_BAND - d) as i32 * 140 / CONE_BAND as i32
                            * fade as i32
                            / 255)
                            .clamp(0, 255) as u32;
                        c = blend_px(c, 0x00_FF_FF_FF, kk);
                    }
                }
            }
            let idx = gy as usize * DISPLAY_W + gx as usize;
            unsafe {
                BACK_BUF[idx] = blend_px(BACK_BUF[idx], c, a);
            }
        }
    }
    true
}

/// Phosphor-Bold 图标字形（笔画加粗）按墨迹包围盒居中绘制到 (cx, cy)，
/// 灰度 alpha 经 OobeSmooth 与背景混合。字形不可用返回 false。
pub(crate) fn draw_icon_thick(cx: i32, cy: i32, cp: u32, px: f32, color: u32) -> bool {
    use crate::sys_nav::glass::SmoothTarget;
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
            let a = g.data[row * g.width + col] as u32;
            if a == 0 {
                continue;
            }
            OobeSmooth.put_aa(x0 + col as i32, y0 + row as i32, color, a);
        }
    }
    true
}

/// 页头：步骤图标（上方偏下）+ 图标下方居中标题 + 副标题。
fn draw_page_header(step: usize, title: &[u8], desc: &[u8]) {
    draw_step_icon(step);
    draw_center_bold_px(PAGE_TITLE_Y, title, FONT_TITLE_PX, C_TEXT);
    draw_center_px(PAGE_DESC_Y, desc, FONT_BODY_PX, C_DIM);
}

// ==================== 各步骤渲染 ====================

// 欢迎页版式（ColorOS 15 风格：Hero 徽标居中，标题/副标题在其下方）。
const WEL_TITLE_Y: i32 = HERO_CY + HERO_R + 26; // 358
const WEL_DESC_Y: i32 = WEL_TITLE_Y + GAP_AFTER_TITLE; // 417

fn render_welcome() {
    draw_brand(1.0);
    draw_center_bold_px(
        WEL_TITLE_Y,
        (if unsafe { LANG == 0 } { "欢迎使用 KunxOS" } else { "Welcome to KunxOS" }).as_bytes(),
        FONT_TITLE_PX,
        C_TEXT,
    );
    let desc: &[u8] = (if unsafe { LANG == 0 } { "你的安全移动操作系统" } else { "Your secure mobile OS" }).as_bytes();
    draw_center_px(WEL_DESC_Y, desc, FONT_BODY_PX, C_DIM);
    // 副标题下方小字（ColorOS 15 欢迎语）。
    draw_center_px(
        WEL_DESC_Y + GAP_AFTER_BODY,
        (if unsafe { LANG == 0 } { "安全护航，简单开始" } else { "Secure by design, simple to start" }).as_bytes(),
        FONT_HINT_PX,
        C_HINT,
    );
}

fn render_language() {
    let (title, desc): (&[u8], &[u8]) = if unsafe { LANG == 0 } {
        ("选择语言".as_bytes(), "设置系统界面语言".as_bytes())
    } else {
        ("Select Language".as_bytes(), "Set the system language".as_bytes())
    };
    draw_page_header(1, title, desc);
    for i in 0..2 {
        let y = WIDGET_Y + i as i32 * CARD_PITCH;
        let label: &[u8] = (if i == 0 { "简体中文" } else { "English" }).as_bytes();
        let selected = unsafe { LANG == i };
        draw_option_card(CARD_X, y, CARD_W, CARD_H, selected);
        draw_radio(OPT_RX, y + OPT_RY, selected, C_TEXT);
        draw_text_px(CARD_X + 20, y + (CARD_H - 17) / 2, label, FONT_BODY_PX, C_TEXT);
    }
}

/// 模块 i 的芯片左上角（列优先：左列 0..10、右列 11..20）。
fn module_pos(i: usize) -> (i32, i32) {
    let col = (i / GRID_ROWS_COL) as i32;
    let row = (i % GRID_ROWS_COL) as i32;
    (
        GRID_X + col * (GRID_COL_W + GRID_GAP),
        WIDGET_Y + row * GRID_PITCH,
    )
}

/// 单个模块芯片：圆角卡 + 右侧复选方框（选中=淡绿底绿描边；锁定=灰字）。
fn draw_module_chip(i: usize) {
    let (gx, gy) = module_pos(i);
    let on = module_on(i);
    let locked = MODULES[i].locked;
    if on && !locked {
        fill_rounded(gx, gy, GRID_COL_W, GRID_ROW_H, GRID_R, C_ACCENT_TINT);
        card_border(gx, gy, GRID_COL_W, GRID_ROW_H, GRID_R, C_ACCENT_STRONG);
    } else {
        fill_rounded(gx, gy, GRID_COL_W, GRID_ROW_H, GRID_R, C_WHITE);
        card_border(gx, gy, GRID_COL_W, GRID_ROW_H, GRID_R, C_CARD_BORDER);
    }
    let c = if locked { C_HINT } else { C_TEXT };
    draw_checkbox(gx + GRID_COL_W - 34, gy + 10, on, c);
    draw_text_px(
        gx + 14,
        gy + (GRID_ROW_H - 17) / 2,
        MODULES[i].display_name().as_bytes(),
        FONT_BODY_PX,
        c,
    );
}

fn render_modules() {
    let (title, desc): (&[u8], &[u8]) = if unsafe { LANG == 0 } {
        (
            "选择应用".as_bytes(),
            "选择需要预装的应用，可在设置中随时更改".as_bytes(),
        )
    } else {
        (
            "Select Apps".as_bytes(),
            "Choose apps to install; change anytime in Settings".as_bytes(),
        )
    };
    draw_page_header(2, title, desc);
    for i in 0..MODULES.len() {
        draw_module_chip(i);
    }
}

fn render_timezone() {
    let (title, desc): (&[u8], &[u8]) = if unsafe { LANG == 0 } {
        ("选择时区".as_bytes(), "时区影响日期与时间显示".as_bytes())
    } else {
        ("Select Timezone".as_bytes(), "Affects date and time display".as_bytes())
    };
    draw_page_header(3, title, desc);
    let tz = unsafe { TIMEZONE };
    for i in 0..8 {
        let y = WIDGET_Y + i as i32 * CARD_PITCH;
        let selected = tz == i;
        draw_option_card(CARD_X, y, CARD_W, CARD_H, selected);
        draw_radio(OPT_RX, y + OPT_RY, selected, C_TEXT);
        draw_text_px(CARD_X + 20, y + (CARD_H - 17) / 2, TIMEZONES[i].as_bytes(), FONT_BODY_PX, C_TEXT);
    }
}

fn render_network() {
    let (title, desc): (&[u8], &[u8]) = if unsafe { LANG == 0 } {
        (
            "网络设置".as_bytes(),
            "建议连接网络，可稍后在设置中更改".as_bytes(),
        )
    } else {
        (
            "Network".as_bytes(),
            "A network is recommended; change anytime in Settings".as_bytes(),
        )
    };
    draw_page_header(4, title, desc);
    // 已连接状态行（绿色圆点 + 居中文本）。
    let s: &[u8] = (if unsafe { LANG == 0 } { "已连接 10.0.2.15" } else { "Connected 10.0.2.15" }).as_bytes();
    let tx = (DISPLAY_W as i32 - text_width_px(s, FONT_BODY_PX)) / 2;
    fill_circle(tx - 16, WIDGET_Y + 8 + 8, 4, C_GREEN);
    draw_text_px(tx, WIDGET_Y + 8, s, FONT_BODY_PX, C_DIM);
    draw_network_skip_row();
}

/// 「跳过网络设置」圆角卡片行（render 与局部重绘共用，必须一致）。
fn draw_network_skip_row() {
    let ry = WIDGET_Y + GAP_AFTER_BODY;
    fill_rounded(CARD_X, ry, CARD_W, CARD_H, CARD_R, C_WHITE);
    card_border(CARD_X, ry, CARD_W, CARD_H, CARD_R, C_CARD_BORDER);
    draw_text_px(
        CARD_X + 20,
        ry + (CARD_H - 17) / 2,
        (if unsafe { LANG == 0 } { "跳过网络设置" } else { "Skip network setup" }).as_bytes(),
        FONT_BODY_PX,
        C_TEXT,
    );
    draw_checkbox(OPT_RX, ry + OPT_RY, unsafe { NET_SKIP }, C_TEXT);
}

fn render_agreement() {
    let (title, desc): (&[u8], &[u8]) = if unsafe { LANG == 0 } {
        ("用户协议".as_bytes(), "".as_bytes())
    } else {
        ("Agreement".as_bytes(), "".as_bytes())
    };
    draw_page_header(5, title, desc);
    let lines: [&[u8]; 3] = [
        (if unsafe { LANG == 0 } { "1. 我们尽量少收集数据，数据保留在本机。" } else { "1. We collect minimal data; stored locally." }).as_bytes(),
        (if unsafe { LANG == 0 } { "2. 云服务依赖 KunxOS 服务器，可能不稳定。" } else { "2. Cloud features rely on KunxOS servers." }).as_bytes(),
        (if unsafe { LANG == 0 } { "3. 部分组件开源，遵循 MIT 许可证。" } else { "3. Some components are open source (MIT)." }).as_bytes(),
    ];
    for (i, l) in lines.iter().enumerate() {
        draw_center_px(WIDGET_Y + i as i32 * GAP_AFTER_BODY, l, FONT_BODY_PX, C_DIM);
    }
    draw_agreement_row();
    draw_center_px(
        WIDGET_Y + 3 * GAP_AFTER_BODY + CARD_H + GAP_AFTER_HINT,
        (if unsafe { LANG == 0 } { "（可选，不勾选也可使用）" } else { "(Optional)" }).as_bytes(),
        FONT_HINT_PX,
        C_HINT,
    );
}

/// 「我已阅读并同意」圆角卡片行（render 与局部重绘共用，必须一致）。
fn draw_agreement_row() {
    let cy = WIDGET_Y + 3 * GAP_AFTER_BODY;
    fill_rounded(CARD_X, cy, CARD_W, CARD_H, CARD_R, C_WHITE);
    card_border(CARD_X, cy, CARD_W, CARD_H, CARD_R, C_CARD_BORDER);
    draw_text_px(
        CARD_X + 20,
        cy + (CARD_H - 17) / 2,
        (if unsafe { LANG == 0 } { "我已阅读并同意" } else { "I have read and agree" }).as_bytes(),
        FONT_BODY_PX,
        C_TEXT,
    );
    draw_checkbox(OPT_RX, cy + OPT_RY, unsafe { AGREEMENT }, C_TEXT);
}

fn render_account() {
    let (title, desc): (&[u8], &[u8]) = if unsafe { LANG == 0 } {
        ("创建账户".as_bytes(), "设置本地用户名".as_bytes())
    } else {
        ("Create Account".as_bytes(), "Set a local username".as_bytes())
    };
    draw_page_header(6, title, desc);
    draw_text_px(
        FIELD_X,
        WIDGET_Y,
        (if unsafe { LANG == 0 } { "用户名" } else { "Username" }).as_bytes(),
        FONT_HINT_PX,
        C_HINT,
    );
    let ub = unsafe { &USER_BUF[..USER_LEN] };
    let f0 = unsafe { FIELD_FOCUS == 0 };
    draw_field(
        FIELD_X,
        FIELD_Y0,
        ub,
        f0,
        false,
        (if unsafe { LANG == 0 } { "输入用户名" } else { "Enter username" }).as_bytes(),
    );
    draw_text_px(
        FIELD_X,
        FIELD_Y0 + FIELD_H + GAP_AFTER_HINT,
        (if unsafe { LANG == 0 } { "密码与锁屏将在后续步骤设置" } else { "Passcode and lock screen come next" }).as_bytes(),
        FONT_HINT_PX,
        C_HINT,
    );
}

/// 绘制一个输入框：白色底 + 边框（聚焦为强调色）；空内容显示 15px 占位提示
/// （#8E8E93），有内容以 17px 正文显示（密码掩码为 `*`，聚焦时末尾加 `_` 光标）。
fn draw_field(x: i32, y: i32, s: &[u8], focused: bool, masked: bool, ph: &[u8]) {
    // ColorOS 15 输入框：白色圆角底 + 中性灰描边；聚焦 = 淡绿底 + 绿描边。
    if focused {
        fill_rounded(x, y, FIELD_W, FIELD_H, 16, C_ACCENT_TINT);
        card_border(x, y, FIELD_W, FIELD_H, 16, C_ACCENT_STRONG);
    } else {
        fill_rounded(x, y, FIELD_W, FIELD_H, 16, C_WHITE);
        card_border(x, y, FIELD_W, FIELD_H, 16, C_CARD_BORDER);
    }
    if s.is_empty() {
        draw_text_px(x + 12, y + (FIELD_H - 17) / 2, ph, FONT_HINT_PX, C_HINT);
        return;
    }
    let mut buf = [0u8; 20];
    let mut n = 0usize;
    for &c in s {
        buf[n] = if masked { b'*' } else { c };
        n += 1;
        if n >= buf.len() {
            break;
        }
    }
    if focused && n < buf.len() {
        buf[n] = b'_';
        n += 1;
    }
    draw_text_px(x + 12, y + (FIELD_H - 17) / 2, &buf[..n], FONT_BODY_PX, C_TEXT);
}

/// 锁屏方式选择（PIN / 图案 / 密码）。
fn render_security() {
    let (title, desc): (&[u8], &[u8]) = if unsafe { LANG == 0 } {
        ("选择锁屏方式".as_bytes(), "选择解锁时的验证方式".as_bytes())
    } else {
        ("Lock Screen".as_bytes(), "Choose how to unlock".as_bytes())
    };
    draw_page_header(7, title, desc);
    let names: [&[u8]; 3] = ["PIN（数字密码）".as_bytes(), "图案（3x3）".as_bytes(), "密码（4-16 位）".as_bytes()];
    for i in 0..3 {
        let y = WIDGET_Y + i as i32 * CARD_PITCH;
        let selected = unsafe { SEC_ENABLED && SEC_TYPE == i };
        draw_option_card(CARD_X, y, CARD_W, CARD_H, selected);
        draw_radio(OPT_RX, y + OPT_RY, selected, C_TEXT);
        draw_text_px(CARD_X + 20, y + (CARD_H - 17) / 2, names[i], FONT_BODY_PX, C_TEXT);
    }
}

/// 锁屏密码设置（密码 + 二次确认，掩码显示）。
fn render_password() {
    let (title, desc): (&[u8], &[u8]) = if unsafe { LANG == 0 } {
        ("设置锁屏密码".as_bytes(), "输入 4-16 位密码并再次确认".as_bytes())
    } else {
        ("Set Passcode".as_bytes(), "Enter a 4-16 digit passcode twice".as_bytes())
    };
    draw_page_header(8, title, desc);
    draw_text_px(
        FIELD_X,
        WIDGET_Y,
        (if unsafe { LANG == 0 } { "密码（4-16 位）" } else { "Passcode (4-16 digits)" }).as_bytes(),
        FONT_HINT_PX,
        C_HINT,
    );
    let pb = unsafe { &PASS_BUF[..PASS_LEN] };
    let f0 = unsafe { FIELD_FOCUS == 0 };
    draw_field(
        FIELD_X,
        FIELD_Y0,
        pb,
        f0,
        true,
        (if unsafe { LANG == 0 } { "输入密码" } else { "Enter passcode" }).as_bytes(),
    );
    draw_text_px(
        FIELD_X,
        FIELD_Y0 + FIELD_H + GAP_AFTER_HINT,
        (if unsafe { LANG == 0 } { "确认密码" } else { "Confirm passcode" }).as_bytes(),
        FONT_HINT_PX,
        C_HINT,
    );
    let cb = unsafe { &CONFIRM_BUF[..CONFIRM_LEN] };
    let f1 = unsafe { FIELD_FOCUS == 1 };
    draw_field(
        FIELD_X,
        FIELD_Y1,
        cb,
        f1,
        true,
        (if unsafe { LANG == 0 } { "再次输入密码" } else { "Re-enter passcode" }).as_bytes(),
    );
    draw_text_px(
        FIELD_X,
        FIELD_Y1 + FIELD_H + GAP_AFTER_HINT,
        (if unsafe { LANG == 0 } { "解锁时输入同一密码，请牢记" } else { "Use this passcode to unlock; remember it" }).as_bytes(),
        FONT_HINT_PX,
        C_HINT,
    );
}

/// 胁迫密码设置（胁迫密码 + 二次确认，掩码显示）。
fn render_duress() {
    let (title, desc): (&[u8], &[u8]) = if unsafe { LANG == 0 } {
        ("设置胁迫密码".as_bytes(), "被胁迫时输入的隐蔽密码".as_bytes())
    } else {
        ("Set Duress PIN".as_bytes(), "A hidden passcode for duress".as_bytes())
    };
    draw_page_header(9, title, desc);
    draw_text_px(
        FIELD_X,
        WIDGET_Y,
        (if unsafe { LANG == 0 } { "胁迫密码（4-16 位）" } else { "Duress PIN (4-16 digits)" }).as_bytes(),
        FONT_HINT_PX,
        C_HINT,
    );
    let db = unsafe { &DURESS_BUF[..DURESS_LEN] };
    let f0 = unsafe { FIELD_FOCUS == 0 };
    draw_field(
        FIELD_X,
        FIELD_Y0,
        db,
        f0,
        true,
        (if unsafe { LANG == 0 } { "输入胁迫密码" } else { "Enter duress PIN" }).as_bytes(),
    );
    draw_text_px(
        FIELD_X,
        FIELD_Y0 + FIELD_H + GAP_AFTER_HINT,
        (if unsafe { LANG == 0 } { "确认胁迫密码" } else { "Confirm duress PIN" }).as_bytes(),
        FONT_HINT_PX,
        C_HINT,
    );
    let dc = unsafe { &DURESS_CONFIRM_BUF[..DURESS_CONFIRM_LEN] };
    let f1 = unsafe { FIELD_FOCUS == 1 };
    draw_field(
        FIELD_X,
        FIELD_Y1,
        dc,
        f1,
        true,
        (if unsafe { LANG == 0 } { "再次输入胁迫密码" } else { "Re-enter duress PIN" }).as_bytes(),
    );
    draw_text_px(
        FIELD_X,
        FIELD_Y1 + FIELD_H + GAP_AFTER_HINT,
        (if unsafe { LANG == 0 } { "将触发你选择的隐蔽动作" } else { "Triggers your chosen hidden action" }).as_bytes(),
        FONT_HINT_PX,
        C_HINT,
    );
}

// ==================== OriginOS 风格选择列表卡（胁迫动作 / 外观步骤共用） ====================

/// BACK_BUF 抗锯齿绘制目标（列表卡描边 / 单选圈混合用）。
struct OobeDlg;

impl dialog::DialogUi for OobeDlg {
    fn put(&mut self, x: i32, y: i32, color: u32) {
        put_pixel(x, y, color);
    }
    fn back(&self, x: i32, y: i32) -> u32 {
        if x >= 0 && y >= 0 && x < DISPLAY_W as i32 && y < DISPLAY_H as i32 {
            unsafe { BACK_BUF[y as usize * DISPLAY_W + x as usize] }
        } else {
            0
        }
    }
    fn text(&mut self, x: i32, y: i32, s: &[u8], px: f32, color: u32) {
        draw_text_px(x, y, s, px, color);
    }
    fn text_bold(&mut self, x: i32, y: i32, s: &[u8], px: f32, color: u32) {
        draw_text_semibold_px(x, y, s, px, color);
    }
    fn text_w(&self, s: &[u8], px: f32) -> i32 {
        text_width_px(s, px)
    }
    fn text_top(&self, px: f32, cy: i32) -> i32 {
        cy - (px as i32) / 2
    }
}

/// 单张 OriginOS 选择列表卡：白底大圆角 + 行间发丝分割线 + 右侧单选圈。
/// 行高与行距一致（无缝拼接），命中测试沿用 `WIDGET_Y + i * rh` 行矩形。
fn draw_option_list(items: &[&[u8]], selected: usize) {
    let rh = CARD_PITCH; // 行高 = 行距：行与行无缝
    let ch = items.len() as i32 * rh;
    fill_rounded(CARD_X, WIDGET_Y, CARD_W, ch, 24, C_WHITE);
    card_border(CARD_X, WIDGET_Y, CARD_W, ch, 24, C_CARD_BORDER);
    let mut ui = OobeDlg;
    for (i, item) in items.iter().enumerate() {
        let ry = WIDGET_Y + i as i32 * rh;
        if i > 0 {
            for dx in 0..CARD_W {
                put_pixel(CARD_X + dx, ry, C_CARD_BORDER);
            }
        }
        dialog::draw_radio(&mut ui, OPT_RX + 12, ry + rh / 2, selected == i);
        draw_text_px(
            CARD_X + 20,
            ry + (rh - 17) / 2,
            item,
            FONT_BODY_PX,
            if selected == i { C_TEXT } else { C_DIM },
        );
    }
}

/// 胁迫密码触发动作（伪装系统 / 静默擦除 / 发送警报）。
fn render_duress_action() {
    let (title, desc): (&[u8], &[u8]) = if unsafe { LANG == 0 } {
        (
            "胁迫密码触发动作".as_bytes(),
            "选择输入胁迫密码后的行为".as_bytes(),
        )
    } else {
        (
            "Duress Action".as_bytes(),
            "Choose what happens when the duress PIN is used".as_bytes(),
        )
    };
    draw_page_header(10, title, desc);
    let acts: [&[u8]; 3] = [
        (if unsafe { LANG == 0 } { "伪装系统（保持锁定）" } else { "Fake system (stay locked)" }).as_bytes(),
        (if unsafe { LANG == 0 } { "静默擦除（恢复出厂）" } else { "Wipe data (factory reset)" }).as_bytes(),
        (if unsafe { LANG == 0 } { "发送警报（上报安全中心）" } else { "Send alert" }).as_bytes(),
    ];
    draw_option_list(&acts, unsafe { DURESS_ACTION });
}

fn render_theme() {
    let (title, desc): (&[u8], &[u8]) = if unsafe { LANG == 0 } {
        ("选择外观".as_bytes(), "浅色或深色主题".as_bytes())
    } else {
        ("Appearance".as_bytes(), "Light or dark theme".as_bytes())
    };
    draw_page_header(11, title, desc);
    let sel = if unsafe { THEME_DARK } { 1 } else { 0 };
    let labels: [&[u8]; 2] = [
        (if unsafe { LANG == 0 } { "浅色" } else { "Light" }).as_bytes(),
        (if unsafe { LANG == 0 } { "深色" } else { "Dark" }).as_bytes(),
    ];
    draw_option_list(&labels, sel);
    // 颜色预览圆（浅色=白圈灰边；深色=近黑圈），位于文字左侧。
    let rh = CARD_PITCH;
    for i in 0..2 {
        let ry = WIDGET_Y + i as i32 * rh;
        let sx = CARD_X + CARD_W - 220;
        let sy = ry + rh / 2;
        if i == 0 {
            fill_circle(sx, sy, 12, C_WHITE);
            let r = 12;
            for dy in -r..=r {
                for dx in -r..=r {
                    let d = dx * dx + dy * dy;
                    if d <= r * r && d >= (r - 2) * (r - 2) {
                        put_pixel(sx + dx, sy + dy, C_CARD_BORDER);
                    }
                }
            }
        } else {
            fill_circle(sx, sy, 12, C_BTN_PRIMARY);
        }
    }
}

struct OobeCanvas;
impl sys_nav::NavCanvas for OobeCanvas {
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
impl button::ButtonCanvas for OobeCanvas {
    fn put(&mut self, x: i32, y: i32, color: u32) {
        put_pixel(x, y, color);
    }
    fn fill_rounded(&mut self, x: i32, y: i32, w: i32, h: i32, r: i32, color: u32) {
        fill_rounded(x, y, w, h, r, color);
    }
    fn text_width(&self, s: &[u8]) -> i32 {
        text_width_px(s, FONT_BTN_PX)
    }
    fn draw_text(&mut self, x: i32, y: i32, s: &[u8], color: u32) {
        draw_text_semibold_px(x, y, s, FONT_BTN_PX, color);
    }
    fn backdrop(&self, x: i32, y: i32) -> u32 {
        if x >= 0 && y >= 0 && x < DISPLAY_W as i32 && y < DISPLAY_H as i32 {
            unsafe { BACK_BUF[y as usize * DISPLAY_W + x as usize] }
        } else {
            0
        }
    }
}

fn render_complete() {
    // ColorOS 15 完成页：绿对勾圆盘 + 标题 + 白卡摘要。
    fill_circle(270, 212, 34, C_ACCENT);
    draw_line_thick(258, 212, 264, 218, C_WHITE);
    draw_line_thick(264, 218, 282, 200, C_WHITE);
    draw_center_bold_px(278, (if unsafe { LANG == 0 } { "KunxOS 已就绪" } else { "KunxOS is ready" }).as_bytes(), FONT_TITLE_PX, C_TEXT);
    // 摘要卡片
    let ch = 172;
    let cy = 278 + GAP_AFTER_TITLE - 10;
    fill_rounded(CARD_X, cy, CARD_W, ch, CARD_R, C_WHITE);
    card_border(CARD_X, cy, CARD_W, ch, CARD_R, C_CARD_BORDER);
    let ty = cy + 16;
    // 用户名
    let un = unsafe { USER_LEN.min(USER_BUF.len()) };
    let mut buf = [0u8; 40];
    let mut n = 0usize;
    for &c in (if unsafe { LANG == 0 } { "用户：" } else { "User: " }).as_bytes() {
        buf[n] = c;
        n += 1;
    }
    unsafe { buf[n..n + un].copy_from_slice(&USER_BUF[..un]); }
    n += un;
    draw_text_px(CARD_X + 20, ty, &buf[..n], FONT_BODY_PX, C_TEXT);
    // 已选模块数
    let mut cnt = 0usize;
    for i in 0..MODULES.len() {
        if module_on(i) {
            cnt += 1;
        }
    }
    let mut m = 0usize;
    for &c in (if unsafe { LANG == 0 } { "应用模块：" } else { "Modules: " }).as_bytes() {
        buf[m] = c;
        m += 1;
    }
    let mut v = cnt;
    if v == 0 {
        buf[m] = b'0';
        m += 1;
    } else {
        let mut tmp = [0u8; 8];
        let mut ti = tmp.len();
        while v > 0 {
            ti -= 1;
            tmp[ti] = b'0' + (v % 10) as u8;
            v /= 10;
        }
        let c = tmp.len() - ti;
        buf[m..m + c].copy_from_slice(&tmp[ti..]);
        m += c;
    }
    draw_text_px(CARD_X + 20, ty + GAP_AFTER_BODY, &buf[..m], FONT_BODY_PX, C_TEXT);
    // 锁屏方式 + 胁迫动作摘要
    let lock_name: &[u8] = unsafe {
        match SEC_TYPE {
            1 => if LANG == 0 { "锁屏：图案".as_bytes() } else { "Lock: Pattern".as_bytes() },
            2 => if LANG == 0 { "锁屏：密码".as_bytes() } else { "Lock: Password".as_bytes() },
            _ => if LANG == 0 { "锁屏：PIN".as_bytes() } else { "Lock: PIN".as_bytes() },
        }
    };
    let duress_name: &[u8] = unsafe {
        match DURESS_ACTION {
            1 => if LANG == 0 { "胁迫：静默擦除".as_bytes() } else { "Duress: Wipe".as_bytes() },
            2 => if LANG == 0 { "胁迫：发送警报".as_bytes() } else { "Duress: Alert".as_bytes() },
            _ => if LANG == 0 { "胁迫：伪装系统".as_bytes() } else { "Duress: Fake".as_bytes() },
        }
    };
    draw_text_px(CARD_X + 20, ty + 2 * GAP_AFTER_BODY, lock_name, FONT_BODY_PX, C_TEXT);
    draw_text_px(CARD_X + 20, ty + 3 * GAP_AFTER_BODY, duress_name, FONT_BODY_PX, C_TEXT);
    draw_center_px(cy + ch + GAP_AFTER_BODY, (if unsafe { LANG == 0 } { "所有配置将被保存" } else { "All config will be saved" }).as_bytes(), FONT_BODY_PX, C_DIM);
}

fn render() {
        draw_background();
        unsafe {
        match STEP {
            0 => render_welcome(),
            1 => render_language(),
            2 => render_modules(),
            3 => render_timezone(),
            4 => render_network(),
            5 => render_agreement(),
            6 => render_account(),
            7 => render_security(),
            8 => render_password(),
            9 => render_duress(),
            10 => render_duress_action(),
            11 => render_theme(),
            _ => render_complete(),
        }
    }
        // 右上角「跳过」胶囊（欢迎页与完成页不显示）。
        draw_skip_button();
        draw_nav();
    // 光标由 render_and_flip 的 cursor_overlay_init 负责（FB 直绘 overlay），
    // 此处不再整屏绘制光标。
}

// ==================== 光标 overlay 快速路径 ====================
//
// 鼠标移动只更新显存 FB 上光标所在的 26x26 小区域（恢复旧位置像素 → 备份
// 新位置像素 → 画光标），不再整屏重绘。此前每次移动都 render()+flip() 整屏
// （含 TTF 文字栅格化）并在串口打印移动日志，QEMU 软件渲染下光标严重不跟手。
// 光标不再画进 BACK_BUF，而是作为 overlay 直接写 FB（与 launcher 同方案）。

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

/// 把 FB 上贴图外扩 1px 的区域保存到 CUR_UNDER。
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

/// 把 CUR_UNDER 写回 FB（擦除旧光标，恢复被覆盖的界面像素）。
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

/// 把指针贴图直接写入显存 FB（tubiao/shubiao.png，热点对齐指针位置）。
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

/// 整屏重绘（render + flip）后调用：备份光标下像素并以 overlay 画回显存，
/// 用脏矩形呈现（光标画在 canvas 上，不呈现则屏幕看不到指针更新）。
fn cursor_overlay_init() {
    if unsafe { MOUSE_SEEN && !FB_PTR.is_null() } {
        unsafe {
            save_cursor_under(CUR_X, CUR_Y);
            draw_cursor_fb();
            let (cx, cy) = (CUR_X, CUR_Y);
            present_rect(
                cursor_sprite::under_x(cx),
                cursor_sprite::under_y(cy),
                cursor_sprite::CUR_UW,
                cursor_sprite::CUR_UH,
            );
        }
    }
}

/// 光标移动快速路径：只更新 FB 上光标所在小区域，不重绘整屏，随后
/// 用旧+新位置的并集小矩形呈现（避免整屏拖动卡顿）。
fn move_cursor_fb() {
    unsafe {
        if !MOUSE_SEEN || FB_PTR.is_null() {
            CURSOR_DIRTY = false;
            return;
        }
        restore_cursor_under();
        let u = (CUR_UNDER_X, CUR_UNDER_Y);
        save_cursor_under(CUR_X, CUR_Y);
        draw_cursor_fb();
        CURSOR_DIRTY = false;
        // 并集矩形：覆盖旧（u）与新（CUR）两个 26x26 光标区域。
        let nx = cursor_sprite::under_x(CUR_X);
        let ny = cursor_sprite::under_y(CUR_Y);
        let x0 = u.0.min(nx);
        let y0 = u.1.min(ny);
        let x1 = u.0.max(nx) + cursor_sprite::CUR_UW;
        let y1 = u.1.max(ny) + cursor_sprite::CUR_UH;
        present_rect(x0, y0, x1 - x0, y1 - y0);
    }
}

/// render + flip + 光标 overlay 重绘（所有整屏更新统一走此入口）。
fn render_and_flip() {
    render();
    flip();
    // 页面已完整呈现在屏上，此时才播图标光锥扫光（扫光同时页面可见）。
    play_pending_cone();
    // 光锥每帧 flip() 整屏覆盖拷贝 BACK_BUF，会把 overlay 写进 FB 的光标
    // 抹掉 —— 光标 overlay 必须在动效结束后重画（同 pulse_selection 收尾），
    // 否则鼠标会话每次步骤切换后光标凭空消失，直到再次移动/点击。
    cursor_overlay_init();
}

// ==================== 动画引擎（对标ColorOS 15，见 docs/animations.md） ====================
//
// 步间转场：旧页交叉淡化 + 新页上滑 16px（EaseInEaseOut 260ms），底部导航面板
// 每帧重绘（动画进度点）；内容区直接合成写入显存 FB，避免整屏二次拷贝。
// 欢迎页：Hero 徽标呼吸入场 + 标题上滑淡入。选项卡片：选中 绿脉冲。

/// 转场用的旧页快照（仅内容区 0..NAV_Y，不含底部导航面板）。
static mut PREV_BUF: [u32; DISPLAY_W * NAV_Y as usize] = [0u32; DISPLAY_W * NAV_Y as usize];

/// 选中脉冲目标矩形（-1 表示无）。
static mut SEL_X: i32 = -1;
static mut SEL_Y: i32 = -1;
static mut SEL_W: i32 = -1;
static mut SEL_H: i32 = -1;

/// 记录下一次选中脉冲的卡片矩形。
fn pulse_target(x: i32, y: i32, w: i32, h: i32) {
    unsafe {
        SEL_X = x;
        SEL_Y = y;
        SEL_W = w;
        SEL_H = h;
    }
}

/// 选中脉冲：卡片矩形内 绿高亮淡出（≈110ms）。写显存 FB，结束后恢复光标。
fn pulse_selection() {
    unsafe {
        if SEL_W <= 0 {
            return;
        }
        // 钳制到屏幕（present_rect 对超界矩形此前会整体丢弃，光标横跨时漏帧）。
        let x0 = SEL_X.max(0);
        let y0 = SEL_Y.max(0);
        let x1 = (SEL_X + SEL_W).min(DISPLAY_W as i32);
        let y1 = (SEL_Y + SEL_H).min(DISPLAY_H as i32);
        if x0 >= x1 || y0 >= y1 {
            SEL_W = -1;
            return;
        }
        let (sx, sy) = (x0, y0);
        let (sw, sh) = (x1 - x0, y1 - y0);
        let pt0 = mono_us();
        loop {
            let t = ((mono_us().saturating_sub(pt0)) as f32 / PULSE_DUR_US as f32).min(1.0);
            let a = ((1.0 - t) * 96.0) as u32;
            // 每帧先从 BACK_BUF 恢复卡片底图再叠加本帧 tint：此前直接在
            // canvas 上逐帧叠加，帧间 tint 累积（96+48+0），脉冲结束后卡片
            // 永久残留约 50% 绿色。
            for dy in 0..sh {
                for dx in 0..sw {
                    fb_set_px(
                        sx + dx,
                        sy + dy,
                        BACK_BUF[(sy + dy) as usize * DISPLAY_W + (sx + dx) as usize],
                    );
                }
            }
            if a > 0 {
                for dy in 0..sh {
                    for dx in 0..sw {
                        if sys_nav::glass::in_round_rect(dx, dy, sw, sh, CARD_R) {
                            let (px, py) = (sx + dx, sy + dy);
                            fb_set_px(px, py, blend_px(fb_px(px, py), C_ACCENT, a));
                        }
                    }
                }
            }
            present_rect(sx, sy, sw, sh);
            pump_input_during_anim();
            if t >= 1.0 {
                break;
            }
        }
        SEL_W = -1;
        cursor_overlay_init();
    }
}

/// 转场期间的底部导航面板（玻璃 + 复位态按钮 + 动画进度点 + 错误横幅）。
fn draw_nav_anim(to: usize, e: f32) {
    draw_glass_style(0, NAV_Y, DISPLAY_W as i32, NAV_H, 0, &STYLE_SHEET);
    btn_reset_all();
    draw_nav_buttons();
    draw_progress_anim(to, e);
    draw_error_banner();
}

/// 步间转场：`from` -> `to`（`reverse` = 返回导航，新页由上滑入）。
fn transition_pages(_from: usize, to: usize, reverse: bool) {
    unsafe {
        // 1. 快照当前内容区（旧页）。
        core::ptr::copy_nonoverlapping(
            BACK_BUF.as_ptr(),
            PREV_BUF.as_mut_ptr(),
            DISPLAY_W * NAV_Y as usize,
        );
        // 2. 渲染新页到 BACK_BUF（内容 + 导航，导航玻璃取景新内容）。
        render();
        btn_reset_all();
    }
    let ny = NAV_Y;
    let t0 = mono_us();
    loop {
        let el = mono_us().saturating_sub(t0);
        let t = (el as f32 / TRANS_DUR_US as f32).min(1.0);
        // ColorOS 步进转场：交叉淡化走平滑阶跃，位移走「上滑回弹」
        // （ease_out_back 末端轻微过冲后回落，页面有呼吸感）。
        let e = ease_step(t);
        let eb = ease_out_back(t);
        let ea = (255.0 * e) as u32;
        let off = if reverse {
            -(TRANS_SLIDE as f32 * (1.0 - eb)) as i32
        } else {
            (TRANS_SLIDE as f32 * (1.0 - eb)) as i32
        };
        // 内容区：旧页淡出、新页淡入并滑入就位。
        for y in 0..ny {
            let sy = y + off;
            for x in 0..DISPLAY_W as i32 {
                let i = y as usize * DISPLAY_W + x as usize;
                let oldc = unsafe { PREV_BUF[i] };
                let newc = if sy >= 0 && sy < ny {
                    unsafe { BACK_BUF[sy as usize * DISPLAY_W + x as usize] }
                } else {
                    oldc
                };
                fb_set_px(x, y, blend_px(oldc, newc, ea));
            }
        }
        // 导航区整行重绘后刷到显存，并呈现本帧（合成器提交 + 翻页）。
        draw_nav_anim(to, e);
        for y in ny..DISPLAY_H as i32 {
            for x in 0..DISPLAY_W as i32 {
                fb_set_px(x, y, unsafe { BACK_BUF[y as usize * DISPLAY_W + x as usize] });
            }
        }
        present_fb();
        pump_input_during_anim();
        // ★ 退出条件（此前改真实时间推进时漏加 → 转场死循环：OOBE 卡死、输入停摆指针消失）
        if t >= 1.0 {
            break;
        }
    }
    render_and_flip();
}

/// 欢迎页入场动画：Hero 徽标呼吸缩放淡入 + 标题/副标题上滑淡入。
fn animate_welcome() {
    let title: &[u8] = if unsafe { LANG == 0 } {
        "欢迎使用 KunxOS".as_bytes()
    } else {
        "Welcome to KunxOS".as_bytes()
    };
    let desc: &[u8] = if unsafe { LANG == 0 } {
        "你的安全移动操作系统".as_bytes()
    } else {
        "Your secure mobile OS".as_bytes()
    };
    let sub: &[u8] = if unsafe { LANG == 0 } {
        "安全护航，简单开始".as_bytes()
    } else {
        "Secure by design, simple to start".as_bytes()
    };
    // 阶段 A：徽标弹跳入场（ColorOS spring-pop：缩放 0.5 → 1.0，
    // ease_out_back 中段轻微过冲后回落，伴随淡入与柔光晕开）。
    {
        let mut frames = 0u32;
        // 静态区域快照（帧率关键）：背景与 nav 在动画中恒定 —— 循环前画
        // 一次，循环内只做行拷贝恢复；每帧真实写入从 1.8MB（全屏）降到
        // ~5 万像素（brand 区域），TCG 下帧率从 ~15fps 拉到 100+fps。
        draw_background();
        draw_nav();
        let mut nav_snap = [[0u32; DISPLAY_W]; NAV_H as usize];
        unsafe {
            for y in 0..NAV_H as usize {
                nav_snap[y].copy_from_slice(
                    &BACK_BUF[(DISPLAY_H as usize - NAV_H as usize + y) * DISPLAY_W
                        ..(DISPLAY_H as usize - NAV_H as usize + y + 1) * DISPLAY_W],
                );
            }
            // brand 区域背景快照（此时 BACK_BUF = 纯背景）
            let bx0 = (DISPLAY_W as i32 / 2 - HERO_R - 50).max(0) as usize;
            let by0 = (HERO_CY - HERO_R - 70).max(0) as usize;
            for y in 0..HERO_BH {
                HERO_BG_SNAP[y * HERO_BW..(y + 1) * HERO_BW].copy_from_slice(
                    &BACK_BUF[(by0 + y) * DISPLAY_W + bx0..(by0 + y) * DISPLAY_W + bx0 + HERO_BW],
                );
            }
        }
        // 动画时钟起点：静态准备（背景/nav/快照）之后 —— 时长预算不含
        // 一次性渲染开销
        let t0 = mono_us();
        // brand 区域背景快照（覆盖光晕 + 垂直位移余量）
        let bx0 = (DISPLAY_W as i32 / 2 - HERO_R - 50).max(0) as usize;
        let bx1 = (DISPLAY_W as i32 / 2 + HERO_R + 50) as usize;
        let by0 = (HERO_CY - HERO_R - 70).max(0) as usize;
        let by1 = (HERO_CY + HERO_R + 70) as usize;
        let bw = bx1 - bx0;
        loop {
            let t = ((mono_us().saturating_sub(t0)) as f32 / HERO_DUR_US as f32).min(1.0);
            let e = ease_out(t);
            let eb = ease_out_back(t);
            // 恢复 brand 区域背景（逐行拷回，行宽 ~288px）
            unsafe {
                for y in by0..by1 {
                    let src = y - by0;
                    BACK_BUF[y * DISPLAY_W + bx0..y * DISPLAY_W + bx1]
                        .copy_from_slice(&HERO_BG_SNAP[src * bw..src * bw + bw]);
                }
            }
            draw_brand_ex(e, 0.5 + 0.5 * eb);
            unsafe {
                let base = (DISPLAY_H as usize - NAV_H as usize) * DISPLAY_W;
                for y in 0..NAV_H as usize {
                    BACK_BUF[base + y * DISPLAY_W..base + (y + 1) * DISPLAY_W]
                        .copy_from_slice(&nav_snap[y]);
                }
            }
            // 动画只有中央 brand 在变：区域拷贝+脏矩形呈现（帧率拉满关键）
            present_region(
                DISPLAY_W as i32 / 2 - HERO_R - 40,
                HERO_CY - HERO_R - 50,
                DISPLAY_W as i32 / 2 + HERO_R + 40,
                HERO_CY + HERO_R + 50,
            );
            pump_input_during_anim();
            frames += 1;
            let _ = frames;
            if t >= 1.0 {
                break;
            }
        }
    }
    // 阶段 B：标题/副标题/标语 错峰上滑淡入（ColorOS 文字逐级浮现：
    // 标题先行，副标题滞后一档，标语最后，位移各自略小营造视差）。
    {
        let t0 = mono_us();
        loop {
        let p = ((mono_us().saturating_sub(t0)) as f32 / TITLE_DUR_US as f32).min(1.0);
        let e0 = ease_out((p / 0.86).min(1.0));
        let e1 = ease_out(((p - 0.14) / 0.86).clamp(0.0, 1.0));
        let e2 = ease_out(((p - 0.28) / 0.86).clamp(0.0, 1.0));
        let dy0 = ((1.0 - e0) * 16.0) as i32;
        let dy1 = ((1.0 - e1) * 12.0) as i32;
        let dy2 = ((1.0 - e2) * 10.0) as i32;
        draw_background();
        draw_brand(1.0);
        draw_center_bold_px_a(WEL_TITLE_Y + dy0, title, FONT_TITLE_PX, C_TEXT, (255.0 * e0) as u32);
        draw_center_px_a(WEL_DESC_Y + dy1, desc, FONT_BODY_PX, C_DIM, (255.0 * e1) as u32);
        draw_center_px_a(WEL_DESC_Y + GAP_AFTER_BODY + dy2, sub, FONT_HINT_PX, C_HINT, (255.0 * e2) as u32);
        draw_nav();
        // 文字区错峰淡入：区域拷贝+脏矩形呈现
        present_region(0, WEL_TITLE_Y - 30, DISPLAY_W as i32, WEL_DESC_Y + GAP_AFTER_BODY + 40);
        pump_input_during_anim();
        if p >= 1.0 {
            break;
        }
        }
    }
    render_and_flip();
}

// ==================== 闲置持续动画（ColorOS 15 常驻动效，脏矩形呈现） ====================
//
// 等待输入期间屏幕不再静止：欢迎页 Hero 徽标呼吸缩放；其余页当前进度点
// 呼吸脉动；卡片上带「进入悬浮态」高亮。全部只重绘小区域并 `present_rect`，
// 避免整屏 canvas→back 拷贝（这是 QEMU 下鼠标拖动卡顿的主因）。

/// 动画帧计数（16ms/帧，三角形振荡做呼吸）。
static mut ANIM_T: u32 = 0;

/// Hero 动画 brand 区域背景快照（静态区域循环前拍一次，每帧行拷贝恢复）。
const HERO_BW: usize = (HERO_R as usize + 50) * 2;
const HERO_BH: usize = (HERO_R as usize + 70) * 2;
static mut HERO_BG_SNAP: [u32; HERO_BW * HERO_BH] = [0; HERO_BW * HERO_BH];

/// 卡片悬停状态（上次所在步骤/索引，-1 = 无）。
static mut HOVER_STEP: i32 = -1;
static mut HOVER_IDX: i32 = -1;

/// 把 BACK_BUF 的 [x,y,w,h] 区域刷入 canvas 并呈现（所有闲置动画统一走此
/// 入口：绘制原语写 BACK_BUF，此处才触碰 canvas —— 避免 canvas 与 BACK_BUF
/// 长期不一致）。
fn present_back_rect(x: i32, y: i32, w: i32, h: i32) {
    // 钳制到 BACK_BUF（DISPLAY_W×DISPLAY_H）与屏幕范围内：调用方传入的区域
    // （如模块页最后一行的 bottom = last+26）可能超出 960 行，直接循环会
    // BACK_BUF 越界 panic（实测：选择应用页点任意一行必崩）。
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + w).min(DISPLAY_W as i32);
    let y1 = (y + h).min(DISPLAY_H as i32);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    for dy in y0..y1 {
        for dx in x0..x1 {
            fb_set_px(
                dx,
                dy,
                unsafe { BACK_BUF[dy as usize * DISPLAY_W + dx as usize] },
            );
        }
    }
    present_rect(x0, y0, x1 - x0, y1 - y0);
}

/// 当前进度圆点中心（与 draw_progress 布局一致）。
fn dot_center(step: usize) -> (i32, i32) {
    let total = 13;
    let gap = 18;
    let start = (DISPLAY_W as i32 - (total - 1) * gap) / 2;
    (start + step as i32 * gap, NAV_Y + 10)
}

/// 闲置呼吸基线快照：呼吸原语写 BACK_BUF 且逐帧在上一帧结果上混合，
/// 不恢复基线会把光晕/呼吸外圈永久烧进底图（光晕逐帧加深成实心绿圈、
/// r=6 外圈残留）。每帧先恢复快照再绘制 —— 绘制结果与首帧完全一致。
const HERO_SNAP_HALF: i32 = 57; // 覆盖 glow_r+1 = HERO_R*1.025+11
static mut HERO_SNAP: [u32; ((HERO_SNAP_HALF * 2 + 1) * (HERO_SNAP_HALF * 2 + 1)) as usize] =
    [0; ((HERO_SNAP_HALF * 2 + 1) * (HERO_SNAP_HALF * 2 + 1)) as usize];
static mut DOT_SNAP: [u32; 15 * 15] = [0; 15 * 15];
static mut HERO_SNAP_OK: bool = false;
static mut DOT_SNAP_OK: bool = false;

/// 页面重渲染后失效闲置快照（主循环每步进入时调用）。
fn idle_snap_invalidate() {
    unsafe {
        HERO_SNAP_OK = false;
        DOT_SNAP_OK = false;
    }
}

/// 进度点呼吸：半径 5..6 脉动（周期 24 帧 ≈ 380ms）。每帧先恢复基线快照
/// 再绘制 —— 此前 r=6 的外圈会永久残留（「基圆始终被覆盖」的假设在
/// r>5 的帧不成立）。
fn dot_breathe(step: usize) {
    let ph = (unsafe { ANIM_T } % 24) as f32 / 24.0;
    let up = if unsafe { ANIM_T % 24 } < 12 {
        ph * 2.0
    } else {
        2.0 - ph * 2.0
    };
    let r = 5 + (up * 1.0) as i32;
    let (cx, cy) = dot_center(step);
    unsafe {
        let mut i = 0usize;
        if !DOT_SNAP_OK {
            for dy in -7i32..=7 {
                for dx in -7i32..=7 {
                    let (x, y) = (cx + dx, cy + dy);
                    DOT_SNAP[i] = if x < 0
                        || y < 0
                        || x >= DISPLAY_W as i32
                        || y >= DISPLAY_H as i32
                    {
                        0
                    } else {
                        BACK_BUF[y as usize * DISPLAY_W + x as usize]
                    };
                    i += 1;
                }
            }
            DOT_SNAP_OK = true;
            i = 0;
        }
        for dy in -7i32..=7 {
            for dx in -7i32..=7 {
                let (x, y) = (cx + dx, cy + dy);
                if x >= 0 && y >= 0 && x < DISPLAY_W as i32 && y < DISPLAY_H as i32 {
                    BACK_BUF[y as usize * DISPLAY_W + x as usize] = DOT_SNAP[i];
                }
                i += 1;
            }
        }
    }
    fill_circle(cx, cy, r, C_ACCENT);
    present_back_rect(cx - 7, cy - 7, 15, 15);
}

/// 欢迎页 Hero 呼吸：徽标整体 0.975..1.025 缩放（周期 40 帧 ≈ 640ms）。
/// 每帧先恢复基线快照再绘制 —— 此前光晕在 BACK_BUF 上逐帧叠加，
/// 1-3 秒内饱和成实心绿圈，且 1.025 缩放的外圈残永久留。
fn hero_breathe() {
    let ph = (unsafe { ANIM_T } % 40) as f32 / 40.0;
    let up = if unsafe { ANIM_T % 40 } < 20 {
        ph * 2.0
    } else {
        2.0 - ph * 2.0
    };
    unsafe {
        let half = HERO_SNAP_HALF;
        let mut i = 0usize;
        if !HERO_SNAP_OK {
            for dy in -half..=half {
                for dx in -half..=half {
                    let (x, y) = (HERO_CX + dx, HERO_CY + dy);
                    HERO_SNAP[i] = if x < 0
                        || y < 0
                        || x >= DISPLAY_W as i32
                        || y >= DISPLAY_H as i32
                    {
                        0
                    } else {
                        BACK_BUF[y as usize * DISPLAY_W + x as usize]
                    };
                    i += 1;
                }
            }
            HERO_SNAP_OK = true;
            i = 0;
        }
        for dy in -half..=half {
            for dx in -half..=half {
                let (x, y) = (HERO_CX + dx, HERO_CY + dy);
                if x >= 0 && y >= 0 && x < DISPLAY_W as i32 && y < DISPLAY_H as i32 {
                    BACK_BUF[y as usize * DISPLAY_W + x as usize] = HERO_SNAP[i];
                }
                i += 1;
            }
        }
    }
    draw_brand_idle(1.0 + 0.025 * up);
    let size = HERO_SNAP_HALF * 2 + 1;
    present_back_rect(
        HERO_CX - HERO_SNAP_HALF,
        HERO_CY - HERO_SNAP_HALF,
        size,
        size,
    );
}

/// 每帧闲置推进：Hero（第 0 步）/ 进度点（其余步）呼吸。
fn animate_idle(step: usize) {
    // 呼吸 ≤30fps 足够（ANIM_T 每 2 循环推进一次）：每帧呼吸都要恢复
    // 13K 像素快照 + 重绘 brand + 区域呈现，是空闲循环的大头 —— 让出
    // 一半循环给光标移动（用户实测移动卡）。
    unsafe {
        ANIM_IDLE_SKIP = !ANIM_IDLE_SKIP;
        if ANIM_IDLE_SKIP {
            return;
        }
        ANIM_T = ANIM_T.wrapping_add(1);
    }
    if step == 0 {
        hero_breathe();
    } else {
        dot_breathe(step);
    }
}

static mut ANIM_IDLE_SKIP: bool = false;

/// 卡片页命中检测（语言/时区/锁屏/胁迫动作）。
fn hover_card_at(step: usize, x: i32, y: i32) -> Option<usize> {
    let count = match step {
        1 => 2,
        3 => 8,
        7 => 3,
        10 => 3,
        _ => return None,
    };
    for i in 0..count {
        let ry = WIDGET_Y + i as i32 * CARD_PITCH;
        if x >= CARD_X && x < CARD_X + CARD_W && y >= ry && y < ry + CARD_H {
            return Some(i);
        }
    }
    None
}

fn card_selected(step: usize, i: usize) -> bool {
    unsafe {
        match step {
            1 => LANG == i,
            3 => TIMEZONE == i,
            7 => SEC_ENABLED && SEC_TYPE == i,
            10 => DURESS_ACTION == i,
            // 外观页：i=0 浅色 / i=1 深色（与 render_theme 一致）。缺失分支
            // 会让 partial_redraw 把新旧卡都画成未选中态（点击无反馈）。
            11 => (i == 1) == THEME_DARK,
            _ => false,
        }
    }
}

fn card_label(step: usize, i: usize) -> &'static [u8] {
    match step {
        1 => {
            if i == 0 {
                "简体中文".as_bytes()
            } else {
                "English".as_bytes()
            }
        }
        3 => TIMEZONES[i].as_bytes(),
        7 => {
            let names: [&[u8]; 3] = [
                "PIN（数字密码）".as_bytes(),
                "图案（3x3）".as_bytes(),
                "密码（4-16 位）".as_bytes(),
            ];
            names[i]
        }
        10 => {
            let acts: [&[u8]; 3] = [
                (if unsafe { LANG == 0 } {
                    "伪装系统（保持锁定）"
                } else {
                    "Fake system (stay locked)"
                })
                .as_bytes(),
                (if unsafe { LANG == 0 } {
                    "静默擦除（恢复出厂）"
                } else {
                    "Wipe data (factory reset)"
                })
                .as_bytes(),
                (if unsafe { LANG == 0 } {
                    "发送警报（上报安全中心）"
                } else {
                    "Send alert"
                })
                .as_bytes(),
            ];
            acts[i]
        }
        11 => {
            if i == 0 {
                "浅色".as_bytes()
            } else {
                "深色".as_bytes()
            }
        }
        _ => &[],
    }
}

/// 外观步骤卡片内的颜色预览圆（浅=白圈灰边；深=近黑），与 render_theme 一致。
fn draw_theme_swatch(sx: i32, sy: i32, dark: bool) {
    if dark {
        fill_circle(sx + 13, sy + 12, 12, C_BTN_PRIMARY);
    } else {
        fill_circle(sx + 13, sy + 12, 12, C_WHITE);
        let r = 12;
        for dy in -r..=r {
            for dx in -r..=r {
                let d = dx * dx + dy * dy;
                if d <= r * r && d >= (r - 2) * (r - 2) {
                    put_pixel(sx + 13 + dx, sy + 12 + dy, C_CARD_BORDER);
                }
            }
        }
    }
}

/// 按卡片页当前选中态整卡重绘（卡片底色/描边 + 单选 + 文本）。
fn redraw_card(step: usize, i: usize) {
    let y = WIDGET_Y + i as i32 * CARD_PITCH;
    let sel = card_selected(step, i);
    draw_option_card(CARD_X, y, CARD_W, CARD_H, sel);
    if step == 11 {
        // 外观卡片：颜色预览圆（浅=白圈灰边；深=近黑）。
        let sx = OPT_RX - 20 - 26;
        draw_theme_swatch(sx, y + OPT_RY, i == 1);
    }
    draw_radio(OPT_RX, y + OPT_RY, sel, C_TEXT);
    draw_text_px(CARD_X + 20, y + (CARD_H - 17) / 2, card_label(step, i), FONT_BODY_PX, C_TEXT);
}

/// 卡片悬停：进入卡片 → 绿描边高亮；离开 → 恢复常态。仅同页生效。
fn hover_tick() {
    unsafe {
        let step = STEP;
        let idx = match hover_card_at(step, CUR_X, CUR_Y) {
            Some(i) => i as i32,
            None => -1,
        };
        // 离开旧卡片（同页才恢复，避免跨页写坏新页）。
        if HOVER_STEP == step as i32 && HOVER_IDX >= 0 && HOVER_IDX != idx {
            redraw_card(step, HOVER_IDX as usize);
            let y = WIDGET_Y + HOVER_IDX * CARD_PITCH;
            present_back_rect(CARD_X, y, CARD_W, CARD_H);
        }
        // 进入新卡片：重绘后在描边上叠加绿高亮。
        if HOVER_STEP == step as i32 && idx >= 0 && idx != HOVER_IDX {
            redraw_card(step, idx as usize);
            let y = WIDGET_Y + idx * CARD_PITCH;
            card_border(CARD_X, y, CARD_W, CARD_H, CARD_R, C_ACCENT);
            present_back_rect(CARD_X, y, CARD_W, CARD_H);
        }
        HOVER_STEP = step as i32;
        HOVER_IDX = idx;
    }
}

/// 当前卡片页选中索引（供选中变化的局部重绘/回退用）。
fn current_selection(step: usize) -> Option<usize> {
    unsafe {
        match step {
            1 => Some(LANG),
            3 => Some(TIMEZONE),
            7 => Some(SEC_TYPE),
            10 => Some(DURESS_ACTION),
            11 => Some(if THEME_DARK { 1 } else { 0 }),
            _ => None,
        }
    }
}

/// 点击消费后的局部重绘：卡片页只重绘选中变化的卡片（旧/新两张）；
/// 复选页只重绘复选框；输入类步骤仍整屏渲染（含字段/提示联动）。
fn partial_redraw(step: usize, sel_before: Option<usize>) {
    match step {
        1 | 3 | 7 | 10 | 11 => {
            let new = current_selection(step);
            let old = sel_before;
            if old != new {
                if let Some(oi) = old {
                    redraw_card(step, oi);
                    let y = WIDGET_Y + oi as i32 * CARD_PITCH;
                    present_back_rect(CARD_X, y, CARD_W, CARD_H);
                }
                if let Some(ni) = new {
                    redraw_card(step, ni);
                    let y = WIDGET_Y + ni as i32 * CARD_PITCH;
                    present_back_rect(CARD_X, y, CARD_W, CARD_H);
                }
            }
        }
        2 => {
            // 模块网格：整组芯片重绘（选中态底色/描边 + 复选框联动）。
            for i in 0..MODULES.len() {
                draw_module_chip(i);
            }
            let (_, ly) = module_pos(MODULES.len() - 1);
            present_back_rect(
                GRID_X,
                WIDGET_Y,
                GRID_COL_W * 2 + GRID_GAP,
                ly + GRID_ROW_H - WIDGET_Y,
            );
        }
        4 => {
            draw_network_skip_row();
            let ry = WIDGET_Y + GAP_AFTER_BODY;
            present_back_rect(CARD_X, ry, CARD_W, CARD_H);
        }
        5 => {
            draw_agreement_row();
            let cy = WIDGET_Y + 3 * GAP_AFTER_BODY;
            present_back_rect(CARD_X, cy, CARD_W, CARD_H);
        }
        _ => {
            render_and_flip();
        }
    }
}

/// 按钮松开回弹：只重绘底部导航区（玻璃 + 按钮 + 进度点），不整屏渲染。
/// 每帧成本从「整屏 render + 2MB 呈现」降到「540×84 导航条 + 脏矩形」。
fn btn_tick_nav_only() {
    btn_tick_all();
    draw_glass_style(0, NAV_Y, DISPLAY_W as i32, NAV_H, 0, &STYLE_SHEET);
    draw_nav_buttons();
    draw_progress();
    draw_error_banner();
    // 导航条从 BACK_BUF 刷入 canvas，再呈现导航条矩形。
    for y in NAV_Y..DISPLAY_H as i32 {
        for x in 0..DISPLAY_W as i32 {
            fb_set_px(x, y, unsafe { BACK_BUF[y as usize * DISPLAY_W + x as usize] });
        }
    }
    present_rect(0, NAV_Y, DISPLAY_W as i32, NAV_H);
}

// ==================== 字符串构建与保存 ====================

fn fnv1a_hex(data: &[u8], out: &mut [u8; 8]) {
    let mut h: u32 = 0x811c9dc5;
    for &b in data {
        h ^= b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    for i in 0..8 {
        let nib = (h >> ((7 - i) * 4)) & 0xF;
        out[i] = HEX[nib as usize];
    }
}

/// FNV-1a 64 位哈希（16 位 hex），与云服务应用/网关/Worker 的令牌哈希一致。
fn fnv1a64_hex(data: &[u8], out: &mut [u8; 16]) {
    let mut h: u64 = 0xCBF2_9CE4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x1_0000_0001_B3);
    }
    for i in 0..16 {
        let nib = (h >> ((15 - i) * 4)) & 0xF;
        out[i] = HEX[nib as usize];
    }
}

/// 构建 modules 或 modules_removed 的逗号分隔串，返回长度。
fn build_modules_str(out: &mut [u8], removed: bool) -> usize {
    let mut n = 0usize;
    for i in 0..MODULES.len() {
        let on = module_on(i);
        if (removed && !on) || (!removed && on) {
            let key = MODULES[i].key.as_bytes();
            if n > 0 {
                out[n] = b',';
                n += 1;
            }
            out[n..n + key.len()].copy_from_slice(key);
            n += key.len();
        }
    }
    n
}

fn save_lang() {
    let v: &[u8] = if unsafe { LANG == 0 } { b"zh_CN" } else { b"en_US" };
    write_file("/data/oobe/lang", v);
}

fn save_modules() {
    let mut buf = [0u8; 256];
    let n = build_modules_str(&mut buf, false);
    write_file("/data/oobe/modules", &buf[..n]);
    let n2 = build_modules_str(&mut buf, true);
    write_file("/data/oobe/modules_removed", &buf[..n2]);
}

fn save_timezone() {
    let tz = unsafe { TIMEZONE };
    write_file("/data/oobe/timezone", TIMEZONES[tz].as_bytes());
}

fn save_agreement() {
    let v: &[u8] = if unsafe { AGREEMENT } { b"1" } else { b"0" };
    write_file("/data/oobe/agreement", v);
}

/// 账户表单校验：用户名非空。不满足时给出横幅提示并返回 false。
fn validate_account() -> bool {
    unsafe {
        if USER_LEN == 0 {
            print("[oobe] 账号：用户名不能为空
");
            show_error("用户名不能为空".as_bytes());
            return false;
        }
    }
    true
}

/// 锁屏密码校验：4-16 位、两次一致。
fn validate_password() -> bool {
    unsafe {
        if PASS_LEN < 4 {
            print("[oobe] 密码太短
");
            show_error("密码至少 4 位".as_bytes());
            return false;
        }
        if PASS_LEN > 16 {
            print("[oobe] 密码过长
");
            show_error("密码最多 16 位".as_bytes());
            return false;
        }
        if CONFIRM_LEN != PASS_LEN
            || &CONFIRM_BUF[..CONFIRM_LEN] != &PASS_BUF[..PASS_LEN]
        {
            print("[oobe] 密码不一致
");
            show_error("两次密码不一致".as_bytes());
            return false;
        }
    }
    true
}

/// 胁迫密码校验：4-16 位、两次一致、且不能与锁屏密码相同。
fn validate_duress() -> bool {
    unsafe {
        if DURESS_LEN < 4 {
            print("[oobe] 胁迫密码太短
");
            show_error("胁迫密码至少 4 位".as_bytes());
            return false;
        }
        if DURESS_LEN > 16 {
            print("[oobe] 胁迫密码过长
");
            show_error("胁迫密码最多 16 位".as_bytes());
            return false;
        }
        if DURESS_CONFIRM_LEN != DURESS_LEN
            || &DURESS_CONFIRM_BUF[..DURESS_CONFIRM_LEN] != &DURESS_BUF[..DURESS_LEN]
        {
            print("[oobe] 胁迫密码不一致
");
            show_error("两次胁迫密码不一致".as_bytes());
            return false;
        }
        if DURESS_LEN == PASS_LEN && &DURESS_BUF[..DURESS_LEN] == &PASS_BUF[..PASS_LEN] {
            print("[oobe] 胁迫密码 == 密码
");
            show_error("胁迫密码不能与锁屏密码相同".as_bytes());
            return false;
        }
    }
    true
}

fn save_account() {
    // local_account = "<用户名>:<fnv1a hex>"（密码哈希在密码步骤补齐，
    // 见 save_password；此处先写用户名部分，保持与云服务约定的文件存在）。
    let mut buf = [0u8; 64];
    let un = unsafe { USER_LEN.min(USER_BUF.len()) };
    let mut n = un;
    unsafe { buf[..n].copy_from_slice(&USER_BUF[..n]); }
    buf[n] = b':';
    n += 1;
    let mut h = [0u8; 8];
    let pl = unsafe { PASS_LEN };
    fnv1a_hex(unsafe { &PASS_BUF[..pl] }, &mut h);
    buf[n..n + 8].copy_from_slice(&h);
    n += 8;
    write_file("/data/oobe/local_account", &buf[..n]);

    let ce: &[u8] = if unsafe { CLOUD_ENABLED } { b"1" } else { b"0" };
    write_file("/data/oobe/cloud_enabled", ce);

    // cloud_control = "enabled:0" 或 "enabled:1,find:1,lock:0,wipe:0"
    let mut cc = [0u8; 40];
    let mut m = 0usize;
    if unsafe { CLOUD_CONTROL } {
        for &c in b"enabled:1,find:" {
            cc[m] = c;
            m += 1;        }
        cc[m] = if unsafe { CLOUD_FIND } { b'1' } else { b'0' };
        m += 1;
        for &c in b",lock:" {
            cc[m] = c;
            m += 1;
        }
        cc[m] = if unsafe { CLOUD_LOCK } { b'1' } else { b'0' };
        m += 1;
        for &c in b",wipe:" {
            cc[m] = c;
            m += 1;
        }
        cc[m] = if unsafe { CLOUD_WIPE } { b'1' } else { b'0' };
        m += 1;
    } else {
        for &c in b"enabled:0" {
            cc[m] = c;
            m += 1;
        }
    }
    write_file("/data/oobe/cloud_control", &cc[..m]);

    let pe: &[u8] = if unsafe { PARENTAL } { b"1" } else { b"0" };
    write_file("/data/oobe/parental_enabled", pe);

    // parental_control = "apps:,time:120,filter:0"
    let mut pc = [0u8; 48];
    let mut k = 0usize;
    for &c in b"apps:" {
        pc[k] = c;
        k += 1;
    }
    // 简化为空限制列表（默认未启用）
    for &c in b",time:" {
        pc[k] = c;
        k += 1;
    }
    let mut t = unsafe { PARENTAL_TIME };
    let mut tmp = [0u8; 8];
    let mut ti = tmp.len();
    if t == 0 {
        ti -= 1;
        tmp[ti] = b'0';
    } else {
        while t > 0 {
            ti -= 1;
            tmp[ti] = b'0' + (t % 10) as u8;
            t /= 10;
        }
    }
    let c = tmp.len() - ti;
    pc[k..k + c].copy_from_slice(&tmp[ti..]);
    k += c;
    for &ch in b",filter:" {
        pc[k] = ch;
        k += 1;
    }
    pc[k] = if unsafe { PARENTAL_FILTER } { b'1' } else { b'0' };
    k += 1;
    write_file("/data/oobe/parental_control", &pc[..k]);
}

/// 锁屏方式步骤：写入 security_type。用户未主动选择安全（SEC_ENABLED=false，
/// 默认/无交互推进）时不写文件 —— launcher 读不到安全配置就不自动上锁，
/// 用户可直接进入系统（安全功能仅在用户主动选择后生效）。
fn save_security_type() {
    if !unsafe { SEC_ENABLED } {
        return;
    }
    let ty: &[u8] = unsafe {
        match SEC_TYPE {
            1 => b"pattern",
            2 => b"password",
            _ => b"pin",
        }
    };
    write_file("/data/oobe/security_type", ty);
}

/// 锁屏密码步骤：写入 security_hash + local_account 哈希部分 + 云令牌。
fn save_password() {
    // security_hash = fnv1a(锁屏密码)，launcher 锁屏校验用。未启用安全时跳过，
    // 避免默认密码把用户挡在锁屏外（local_account 仍照常写入）。
    let mut h = [0u8; 8];
    let pl = unsafe { PASS_LEN };
    fnv1a_hex(unsafe { &PASS_BUF[..pl] }, &mut h);
    if unsafe { SEC_ENABLED } {
        write_file("/data/oobe/security_hash", &h);
    } else {
        // 未启用安全（或空闲自动推进留空）：清空 security_type。此前的
        // save_security_type（步骤 7）可能已写入 pin/pattern/password，
        // 留着会造成「有 security_type 无 security_hash」→ launcher 开机
        // 即锁死且无法解锁。
        write_file("/data/oobe/security_type", b"");
    }

    // local_account = "<用户名>:<fnv1a hex>"（哈希用最终密码补齐，
    // 与云服务应用 /data/oobe/local_account 约定一致）。
    let mut buf = [0u8; 64];
    let un = unsafe { USER_LEN.min(USER_BUF.len()) };
    let mut n = un;
    unsafe { buf[..n].copy_from_slice(&USER_BUF[..n]); }
    buf[n] = b':';
    n += 1;
    buf[n..n + 8].copy_from_slice(&h);
    n += 8;
    write_file("/data/oobe/local_account", &buf[..n]);

    // cloud_account = "" 或 "username:<token>"（token = fnv1a64(用户名:密码:盐)，
    // 与云服务应用约定一致；见 cloud/PROTOCOL.md）。密码此时才最终确定，
    // 故在此步骤生成。
    if unsafe { CLOUD_ENABLED } {
        let mut seed = [0u8; 96];
        unsafe {
            seed[..un].copy_from_slice(&USER_BUF[..un]);
        }
        let mut si = un;
        seed[si] = b':';
        si += 1;
        seed[si..si + pl].copy_from_slice(unsafe { &PASS_BUF[..pl] });
        si += pl;
        for &c in b":kunxos-cloud" {
            seed[si] = c;
            si += 1;
        }
        let mut tok = [0u8; 16];
        fnv1a64_hex(&seed[..si], &mut tok);
        let mut acct = [0u8; 96];
        unsafe {
            acct[..un].copy_from_slice(&USER_BUF[..un]);
        }
        let mut ai = un;
        acct[ai] = b':';
        ai += 1;
        acct[ai..ai + 16].copy_from_slice(&tok);
        ai += 16;
        write_file("/data/oobe/cloud_account", &acct[..ai]);
        // 默认服务器（QEMU slirp 宿主机；真机部署可改 /data/oobe/cloud_server）
        write_file("/data/oobe/cloud_server", b"10.0.2.2:8080");
    } else {
        write_file("/data/oobe/cloud_account", b"");
    }
}

/// 胁迫密码步骤：写入 duress_hash（用户设置的胁迫密码哈希）。未启用安全时跳过。
fn save_duress() {
    if !unsafe { SEC_ENABLED } {
        return;
    }
    let mut d = [0u8; 8];
    let dl = unsafe { DURESS_LEN };
    fnv1a_hex(unsafe { &DURESS_BUF[..dl] }, &mut d);
    write_file("/data/oobe/duress_hash", &d);
}

/// 胁迫动作步骤：写入 duress_action。未启用安全时跳过。
fn save_duress_action() {
    if !unsafe { SEC_ENABLED } {
        return;
    }
    let act: &[u8] = unsafe {
        match DURESS_ACTION {
            1 => b"wipe",
            2 => b"alert",
            _ => b"fake_system",
        }
    };
    write_file("/data/oobe/duress_action", act);
}

fn save_theme() {
    // theme = "dark,wallpaper:2" / "light,wallpaper:N"
    let mut buf = [0u8; 32];
    let mut n = 0usize;
    let mode: &[u8] = if unsafe { THEME_DARK } { b"dark" } else { b"light" };
    buf[..mode.len()].copy_from_slice(mode);
    n += mode.len();
    for &c in b",wallpaper:" {
        buf[n] = c;
        n += 1;
    }
    buf[n] = b'0' + unsafe { WALLPAPER } as u8;
    n += 1;
    write_file("/data/oobe/theme", &buf[..n]);
    // 导航方式："gesture" / "buttons"
    let nav: &[u8] = if unsafe { NAV_MODE == 1 } { b"buttons" } else { b"gesture" };
    write_file("/data/oobe/nav_mode", nav);
}

fn save_step(step: usize) {
    match step {
        1 => save_lang(),
        2 => save_modules(),
        3 => save_timezone(),
        5 => save_agreement(),
        6 => save_account(),
        7 => save_security_type(),
        8 => save_password(),
        9 => save_duress(),
        10 => save_duress_action(),
        11 => save_theme(),
        _ => {}
    }
}

// ==================== 日志 ====================

fn log_step(step: usize) {
    match step {
        0 => print("[oobe] 欢迎使用 KunxOS
"),
        1 => print("[oobe] 第 2/13 步：语言
"),
        2 => print("[oobe] 第 3/13 步：模块
"),
        3 => print("[oobe] 第 4/13 步：时区
"),
        4 => print("[oobe] 第 5/13 步：网络
"),
        5 => print("[oobe] 第 6/13 步：协议
"),
        6 => print("[oobe] 第 7/13 步：账号
"),
        7 => print("[oobe] 第 8/13 步：锁屏方式
"),
        8 => print("[oobe] 第 9/13 步：密码
"),
        9 => print("[oobe] 第 10/13 步：胁迫 PIN
"),
        10 => print("[oobe] 第 11/13 步：胁迫动作
"),
        11 => print("[oobe] 第 12/13 步：外观
"),
        _ => print("[oobe] 第 13/13 步：完成
"),
    }
}

/// 查询网络状态（向 networkd 发送 STATUS，并打印硬编码 QEMU 默认值）。
fn query_network() {
    unsafe {
        let fd = open(NET_PIPE_PATH, O_RDWR);
        if fd >= 0 {
            syscall3(SYS_LSEEK, fd as usize, 0, SEEK_END);
            write_fd(fd as usize, b"STATUS
");
            syscall1(SYS_CLOSE, fd as usize);
        }
    }
    print("[oobe] network STATUS: IP=10.0.2.15 GW=10.0.2.2
");
}

// ==================== 输入事件 ====================

struct InputEvent {
    x: i32,
    y: i32,
    enter: bool,
    back: bool,
    /// 键盘输入的一个字符（仅 STEP=6 账号步骤使用）。
    ch: Option<u8>,
    /// 键盘退格键。
    backspace: bool,
}

// Linux input_event（64 位小端）：timeval 16B + type u16 + code u16 + value i32 = 24B
const EV_KEY: usize = 1;
const EV_REL: usize = 2;
const EV_ABS: usize = 3;
const ABS_X: usize = 0;
const ABS_Y: usize = 1;
const REL_X: usize = 0;
const REL_Y: usize = 1;
const ABS_MT_POSITION_X: usize = 0x35;
const ABS_MT_POSITION_Y: usize = 0x36;
const BTN_TOUCH: usize = 0x14a;
const BTN_LEFT: usize = 0x110;
const KEY_ENTER: usize = 28;
const KEY_BACKSPACE: usize = 14;
const KEY_LEFTSHIFT: usize = 42;
const KEY_RIGHTSHIFT: usize = 54;

/// 键盘 shift 状态（大写/符号）。
static mut SHIFT: bool = false;

/// Linux evdev 键码 → ASCII 字符（未按 shift 处理）。
///
/// evdev 键码按 **QWERTY 物理布局**编号（KEY_Q=16、KEY_W=17…，非字母表
/// 顺序），必须逐键显式映射（与终端应用 key_char 同一张表）。
fn key_to_char(code: usize, shift: bool) -> Option<u8> {
    let c = match code {
        2 => b'1', 3 => b'2', 4 => b'3', 5 => b'4', 6 => b'5',
        7 => b'6', 8 => b'7', 9 => b'8', 10 => b'9', 11 => b'0',
        16 => b'q', 17 => b'w', 18 => b'e', 19 => b'r', 20 => b't',
        21 => b'y', 22 => b'u', 23 => b'i', 24 => b'o', 25 => b'p',
        30 => b'a', 31 => b's', 32 => b'd', 33 => b'f', 34 => b'g',
        35 => b'h', 36 => b'j', 37 => b'k', 38 => b'l',
        44 => b'z', 45 => b'x', 46 => b'c', 47 => b'v', 48 => b'b',
        49 => b'n', 50 => b'm',
        57 => b' ',
        12 => b'-', 13 => b'=',
        52 => b'.', 51 => b',', 53 => b'/',
        77 => b'@',
        _ => return None,
    };
    let c = if shift && c.is_ascii_lowercase() { c - 32 } else { c };
    if shift {
        Some(match c {
            b'1' => b'!', b'2' => b'@', b'3' => b'#', b'4' => b'$', b'5' => b'%',
            b'6' => b'^', b'7' => b'&', b'8' => b'*', b'9' => b'(', b'0' => b')',
            b'-' => b'_', b'=' => b'+', b'.' => b'>', b',' => b'<', b'/' => b'?',
            _ => c,
        })
    } else {
        Some(c)
    }
}

/// 鼠标指针状态：位置复用 CUR_X/CUR_Y（屏幕像素，相对移动累积）。
static mut MOUSE_DOWN: bool = false;
static mut MOUSE_SEEN: bool = true; // 默认显示指针，鼠标移动后跟随
/// 指针移动标记：等待循环据此重绘（OOBE 每步只渲染一次，需主动刷新光标）。
static mut CURSOR_DIRTY: bool = false;

static mut INPUT_FD: isize = -1;
static mut CUR_X: i32 = DISPLAY_W as i32 / 2; // 屏幕中心
static mut CUR_Y: i32 = DISPLAY_H as i32 / 2;
static mut TOUCH_DOWN: bool = false;

/// 输入调试日志：[oobe] input: x=..., y=..., type=...
fn log_input(kind: &str, x: i32, y: i32) {
    print("[oobe] input: x=");
    print_dec(x as usize);
    print(", y=");
    print_dec(y as usize);
    print(", type=");
    print(kind);
    print("
");
}

/// 输入累积缓冲：事件可能跨 read 分割（单次 read 未必返回完整 24 字节），
/// 先把零散字节累积进缓冲，凑满一个 `input_event` 后再解析，避免事件被丢弃。
static mut EV_BUF: [u8; 64] = [0u8; 64];
static mut EV_LEN: usize = 0;
/// 「最近有原始输入数据」标记：read 返回 >0 字节即置位（含纯指针移动）。
/// 空闲自动推进计时器消费此标记——用户持续移动鼠标（不点击）也应视为
/// 在场，不应被 30 秒无输入判定强推到下一步。
static mut ANY_INPUT_RECENT: bool = false;

/// 解析并消费缓冲头部的单个 input_event（24B）；是完整点击（按下+抬起）或
/// 回车键或返回按钮按下时返回 true，并把动作写入 ev。无论结果如何都消费该事件
/// （剩余字节前移，EV_LEN -= 24），保证连续事件流不被重复解析。
unsafe fn consume_input_event(ev: &mut InputEvent) -> bool {
    let buf = &EV_BUF[..24];
    let etype = u16::from_le_bytes([buf[16], buf[17]]) as usize;
    let code = u16::from_le_bytes([buf[18], buf[19]]) as usize;
    let value = i32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]);

    let mut click = false;
    // 每次解析前清空事件动作（read_input_event 复用同一结构体）。
    ev.enter = false;
    ev.back = false;
    ev.ch = None;
    ev.backspace = false;
    if etype == EV_REL {
        // 鼠标相对移动：累积到指针位置并钳制在屏幕内。
        match code {
            REL_X => CUR_X = (CUR_X + value).clamp(0, DISPLAY_W as i32 - 1),
            REL_Y => CUR_Y = (CUR_Y + value).clamp(0, DISPLAY_H as i32 - 1),
            _ => {}
        }
        MOUSE_SEEN = true;
        CURSOR_DIRTY = true;
    } else if etype == EV_ABS {
        match code {
            ABS_X => {
                CUR_X = value.clamp(0, DISPLAY_W as i32 - 1);
                CURSOR_DIRTY = true;
            }
            ABS_Y => {
                CUR_Y = value.clamp(0, DISPLAY_H as i32 - 1);
                // 注意：移动不打印日志 —— 每帧阻塞串口输出是鼠标卡顿的主因
                CURSOR_DIRTY = true;
            }
            ABS_MT_POSITION_X => {
                CUR_X = value.clamp(0, DISPLAY_W as i32 - 1);
                CURSOR_DIRTY = true;
            }
            ABS_MT_POSITION_Y => {
                CUR_Y = value.clamp(0, DISPLAY_H as i32 - 1);
                CURSOR_DIRTY = true;
            }
            _ => {}
        }
    } else if etype == EV_KEY {
        if code == BTN_LEFT {
            // 鼠标左键：按下记录并触发按钮按压反馈，抬起在指针处产生一次点击。
            if value == 1 {
                MOUSE_DOWN = true;
                log_input("down", CUR_X, CUR_Y);
                btn_on_press(CUR_X, CUR_Y);
            } else if value == 0 && MOUSE_DOWN {
                log_input("up", CUR_X, CUR_Y);
                MOUSE_DOWN = false;
                btn_on_release();
                ev.x = CUR_X;
                ev.y = CUR_Y;
                ev.enter = false;
                ev.back = btn_back_hit(CUR_X, CUR_Y);
                click = true;
            }
        } else if code == BTN_TOUCH {
            if value == 1 {
                TOUCH_DOWN = true;
                log_input("down", CUR_X, CUR_Y);
                btn_on_press(CUR_X, CUR_Y);
            } else if value == 0 {
                log_input("up", CUR_X, CUR_Y);
                if TOUCH_DOWN {
                    TOUCH_DOWN = false;
                    btn_on_release();
                    ev.x = CUR_X;
                    ev.y = CUR_Y;
                    ev.enter = false;
                    ev.back = btn_back_hit(CUR_X, CUR_Y);
                    click = true;
                }
            }
        } else if code == KEY_ENTER {
            // 键盘回车：按下即下一步（账号步骤内为焦点推进，见 wait_for_tap）。
            log_input("key-enter", 0, 0);
            if value == 1 {
                ev.x = 0;
                ev.y = 0;
                ev.enter = true;
                click = true;
            }
        } else if code == KEY_BACKSPACE {
            if value == 1 {
                ev.backspace = true;
                click = true;
            }
        } else if code == KEY_LEFTSHIFT || code == KEY_RIGHTSHIFT {
            SHIFT = value != 0;
        } else if value == 1 {
            // 其它按键（字符键）：按下时翻译成字符。
            if let Some(c) = key_to_char(code, SHIFT) {
                ev.ch = Some(c);
                click = true;
                print("[oobe] key code=");
                print_dec(code);
                print(" ch='");
                write_fd(1, &[c]);
                print("'
");
            }
        }
    }

    // 消费已解析的 24 字节，剩余字节前移。
    let rest = EV_LEN - 24;
    for i in 0..rest {
        EV_BUF[i] = EV_BUF[24 + i];
    }
    EV_LEN = rest;
    click
}

/// 读取一个输入事件；仅在检测到一次完整点击（按下并抬起）或回车键按下时返回 true，
/// 并把点击坐标/回车标志写入 ev。设备未就绪时返回 false。
fn read_input_event(ev: &mut InputEvent) -> bool {
        unsafe {
        if INPUT_FD < 0 {
            INPUT_FD = open(INPUT_PATH, O_RDONLY | O_NONBLOCK);
            if INPUT_FD >= 0 {
                print("[input] /dev/input/event0 就绪
");
            }
        }
        if INPUT_FD < 0 {
            return false;
        }

        loop {
            if EV_LEN >= 24 {
                // 已累积出完整事件：解析并消费；命中点击/回车立即返回。
                if consume_input_event(ev) {
                    return true;
                }
                continue;
            }
            // 缓冲不足一个事件：把新数据追加到缓冲尾部（非阻塞，无数据即返回）。
            let space = EV_BUF.len() - EV_LEN;
            if space == 0 {
                return false;
            }
            let n = syscall3(
                SYS_READ,
                INPUT_FD as usize,
                EV_BUF.as_ptr().add(EV_LEN) as usize,
                space,
            );
            if (n as isize) <= 0 {
                return false; // 无数据（非阻塞 EAGAIN）或读失败
            }
            EV_LEN += n;
            unsafe { ANY_INPUT_RECENT = true; }
        }
    }
}

/// 处理点击：返回 1=下一步、-1=上一步、2=本步选项被消费（已重绘状态）、
/// 3=跳过（右上角）、0=未命中。
fn handle_tap(x: i32, y: i32) -> i32 {
    print("[oobe] 点击 (");
    print_dec(x as usize);
    print(",");
    print_dec(y as usize);
    print("），检查按钮...
");
    unsafe {
        // 跳过按钮（右上角胶囊）：放弃当前步，直接进入完成页。
        if btn_skip_hit(x, y) {
            print("[oobe] 已点击跳过按钮 -> 完成页
");
            return 3;
        }
        // 上一步按钮（次要，底部左侧）：组件命中区。
        if btn_back_hit(x, y) {
            print("[oobe] 已点击返回按钮
");
            return -1;
        }
        // 下一步 / 进入系统（主，底部右侧）：组件命中区。
        let nav_btn: &button::Button = if STEP < 12 { &BTN_NEXT } else { &BTN_ENTER };
        if nav_btn.enabled() && nav_btn.hit(x, y) {
            print("[oobe] 已点击下一步按钮 -> 下一步
");
            return 1;
        }
    }
    let step = unsafe { STEP };
    let r = match step {
        1 => tap_language(x, y),
        2 => tap_modules(x, y),
        3 => tap_timezone(x, y),
        4 => tap_network(x, y),
        5 => tap_agreement(x, y),
        6 => tap_account(x, y),
        7 => tap_security(x, y),
        8 => tap_password(x, y),
        9 => tap_duress(x, y),
        10 => tap_duress_action(x, y),
        11 => tap_theme(x, y),
        _ => 0,
    };
    if r == 2 {
        print("[oobe] 选项已消费
");
    }
    r
}

/// 语言步骤：两个白色卡片单选（右侧 绿圆点）。
fn tap_language(x: i32, y: i32) -> i32 {
    for i in 0..2 {
        let ry = WIDGET_Y + i as i32 * CARD_PITCH;
        if x >= CARD_X && x < CARD_X + CARD_W && y >= ry && y < ry + CARD_H {
            unsafe { LANG = i; }
            pulse_target(CARD_X, ry, CARD_W, CARD_H);
            return 2;
        }
    }
    0
}

/// 模块步骤：勾选/取消勾选应用（两列圆角芯片网格，锁定项忽略）。
fn tap_modules(x: i32, y: i32) -> i32 {
    for i in 0..MODULES.len() {
        let (gx, gy) = module_pos(i);
        if x >= gx && x < gx + GRID_COL_W && y >= gy && y < gy + GRID_ROW_H {
            if !MODULES[i].locked {
                unsafe { MODULE_ON[i] = !MODULE_ON[i]; }
            }
            return 2; // 锁定项也消费点击（不改变状态）
        }
    }
    0
}

/// 时区步骤：8 个白色卡片单选（右侧 绿圆点）。
fn tap_timezone(x: i32, y: i32) -> i32 {
    if x >= CARD_X && x < CARD_X + CARD_W {
        for i in 0..8 {
            let ry = WIDGET_Y + i as i32 * CARD_PITCH;
            if y >= ry && y < ry + CARD_H {
                unsafe { TIMEZONE = i; }
                pulse_target(CARD_X, ry, CARD_W, CARD_H);
                return 2;
            }
        }
    }
    0
}

/// 网络步骤：「跳过网络设置」圆角卡片行（右侧复选方框）。
fn tap_network(x: i32, y: i32) -> i32 {
    let ry = WIDGET_Y + GAP_AFTER_BODY;
    if x >= CARD_X && x < CARD_X + CARD_W && y >= ry && y < ry + CARD_H {
        unsafe { NET_SKIP = !NET_SKIP; }
        return 2;
    }
    0
}

/// 协议步骤：「我已阅读并同意」圆角卡片行（右侧复选方框）。
fn tap_agreement(x: i32, y: i32) -> i32 {
    let cy = WIDGET_Y + 3 * GAP_AFTER_BODY; // 与 render_agreement 一致
    if x >= CARD_X && x < CARD_X + CARD_W && y >= cy && y < cy + CARD_H {
        unsafe { AGREEMENT = !AGREEMENT; }
        return 2;
    }
    0
}

/// 账号步骤：用户名输入框焦点。
fn tap_account(x: i32, y: i32) -> i32 {
    if x >= FIELD_X && x < FIELD_X + FIELD_W && y >= FIELD_Y0 && y < FIELD_Y0 + FIELD_H {
        unsafe { FIELD_FOCUS = 0; }
        return 2;
    }
    0
}

/// 锁屏方式步骤：三种方式卡片单选（右侧 绿圆点）。
fn tap_security(x: i32, y: i32) -> i32 {
    if x >= CARD_X && x < CARD_X + CARD_W {
        for i in 0..3 {
            let ry = WIDGET_Y + i as i32 * CARD_PITCH;
            if y >= ry && y < ry + CARD_H {
                unsafe { SEC_TYPE = i; SEC_ENABLED = true; }
                pulse_target(CARD_X, ry, CARD_W, CARD_H);
                return 2;
            }
        }
    }
    0
}

/// 锁屏密码步骤：两个输入框焦点切换。
fn tap_password(x: i32, y: i32) -> i32 {
    let field_y = [FIELD_Y0, FIELD_Y1];
    for (i, fy) in field_y.iter().enumerate() {
        if x >= FIELD_X && x < FIELD_X + FIELD_W && y >= *fy && y < *fy + FIELD_H {
            unsafe { FIELD_FOCUS = i as i32; }
            return 2;
        }
    }
    0
}

/// 胁迫密码步骤：两个输入框焦点切换。
fn tap_duress(x: i32, y: i32) -> i32 {
    let field_y = [FIELD_Y0, FIELD_Y1];
    for (i, fy) in field_y.iter().enumerate() {
        if x >= FIELD_X && x < FIELD_X + FIELD_W && y >= *fy && y < *fy + FIELD_H {
            unsafe { FIELD_FOCUS = i as i32; }
            return 2;
        }
    }
    0
}

/// 胁迫动作步骤：单选列表卡（行高与行距一致，见 draw_option_list）。
fn tap_duress_action(x: i32, y: i32) -> i32 {
    if x >= CARD_X && x < CARD_X + CARD_W {
        for i in 0..3 {
            let ry = WIDGET_Y + i as i32 * CARD_PITCH;
            if y >= ry && y < ry + CARD_PITCH {
                unsafe { DURESS_ACTION = i; }
                pulse_target(CARD_X, ry, CARD_W, CARD_PITCH);
                return 2;
            }
        }
    }
    0
}

/// 主题步骤：浅色 / 深色单选列表卡（含颜色预览圆）。
fn tap_theme(x: i32, y: i32) -> i32 {
    if x >= CARD_X && x < CARD_X + CARD_W {
        for i in 0..2 {
            let ry = WIDGET_Y + i as i32 * CARD_PITCH;
            if y >= ry && y < ry + CARD_PITCH {
                unsafe { THEME_DARK = i == 1; }
                pulse_target(CARD_X, ry, CARD_W, CARD_PITCH);
                return 2;
            }
        }
    }
    0
}

/// 当前步骤是否需要输入框编辑（账号=用户名；密码/胁迫=双字段）。
fn step_has_fields(step: usize) -> bool {
    matches!(step, 6 | 8 | 9)
}

/// 按步骤校验输入：账号=用户名非空；密码=4-16 位且两次一致；
/// 胁迫=4-16 位、两次一致且不同于锁屏密码。
fn validate_step(step: usize) -> bool {
    match step {
        6 => validate_account(),
        8 => validate_password(),
        9 => validate_duress(),
        _ => true,
    }
}

/// 阻塞等待用户点击「下一步」或按下回车。
///
/// 返回：1 = 用户点击「下一步」/回车；-1 = 返回；3 = 跳过；
/// 4 = 30 秒无输入自动推进（主循环据此应用该步默认选择），
/// 保证 run-gui（/dev/input/event0 可打开但无串口触摸事件）下 OOBE 不会永久挂起。
fn wait_for_tap_next_button() -> i32 {
    let mut event = InputEvent {
        x: 0,
        y: 0,
        enter: false,
        back: false,
        ch: None,
        backspace: false,
    };
    let mut idle; // 空闲毫秒（真实时间，每帧由 loop_t0 重算）
            let mut loop_t0 = mono_us();
    loop {
        err_ui::err_tick();
        if read_input_event(&mut event) {
            loop_t0 = mono_us(); // 有输入：重置真实时间空闲计时的锚点
            if event.enter {
                // 输入框步骤：回车推进焦点；最后一个输入框回车 = 下一步。
                if step_has_fields(unsafe { STEP }) {
                    let f = unsafe { FIELD_FOCUS };
                    let cnt = step_field_count();
                    if f >= 0 && (f as usize) < cnt - 1 {
                        unsafe { FIELD_FOCUS = f + 1; }
                        print("[oobe] 焦点 -> 下一输入框
");
                        render_and_flip();
                        continue;
                    }
                    // 已在最后一个输入框回车：先校验再放行。
                    if !validate_step(unsafe { STEP }) {
                        continue;
                    }
                }
                print("[oobe] 回车键 -> 下一步
");
                return 1;
            }
            if event.backspace {
                if step_has_fields(unsafe { STEP }) {
                    let f = unsafe { FIELD_FOCUS };
                    if f >= 0 {
                        delete_field_char(f as usize);
                        print("[oobe] 退格
");
                        render_and_flip();
                    }
                }
                continue;
            }
            if let Some(c) = event.ch {
                if step_has_fields(unsafe { STEP }) {
                    let f = unsafe { FIELD_FOCUS };
                    if f >= 0 {
                        append_field_char(f as usize, c);
                        print("[oobe] 已输入字符
");
                        render_and_flip();
                    }
                }
                continue;
            }
            if event.back {
                print("[oobe] 已点击返回按钮
");
                return -1;
            }
            let sel_before = current_selection(unsafe { STEP });
            let r = handle_tap(event.x, event.y);
            if r == 1 {
                // 输入框步骤点击「下一步」：先校验再放行。
                if step_has_fields(unsafe { STEP }) && !validate_step(unsafe { STEP }) {
                    continue;
                }
                return 1;
            }
            if r == -1 {
                return -1;
            }
            if r == 3 {
                // 右上角「跳过」：交给主流程处理（放弃当前步直达完成页）。
                return 3;
            }
            if r == 2 {
                // 本步选项被消费：只局部重绘变化区域（卡片/复选），不再整屏
                // render + 2MB 呈现（否则每点一下都卡）；随后播放选中脉冲。
                partial_redraw(unsafe { STEP }, sel_before);
                pulse_selection();
            }
        } else {
            // 指针移动：overlay 快速路径只更新光标所在小区域，
            // 不再整屏 render()+flip()（TTF 重栅格化是鼠标卡顿的主因）。
            if unsafe { CURSOR_DIRTY } {
                move_cursor_fb();
            }
            // 卡片悬停高亮（同页生效，脏矩形呈现）。
            hover_tick();
            // 闲置持续动画：欢迎页 Hero 呼吸 / 其余页进度点呼吸。
            animate_idle(unsafe { STEP });
            // 按钮松开回弹过渡（≈150ms）：无输入时逐帧推进，
            // 只重绘导航条（不整屏 render + 2MB 呈现，避免卡顿）。
            if btn_animating() {
                btn_tick_nav_only();
            }
            // 光标流畅优先（用户实测移动卡）：有输入待处理/光标待更新时
            // 帧间不睡眠；静态空闲 8ms 让出。
            if !unsafe { CURSOR_DIRTY } && !unsafe { ANY_INPUT_RECENT } && !btn_animating() {
                sleep_ms(8);
            }
            // 纯指针移动（不构成点击）也算用户在场：重置真实时间空闲计时
            if unsafe { ANY_INPUT_RECENT } {
                unsafe { ANY_INPUT_RECENT = false };
                loop_t0 = mono_us();
            }
            idle = (mono_us().saturating_sub(loop_t0) / 1_000) as usize;
            if idle >= 30000 {
                print("[oobe] 30 秒无输入，应用默认选择 -> 下一步
");
                return 4;
            }
        }
    }
}

/// 各步点击「下一步」前的默认选择处理。
fn apply_step_defaults(step: usize) {
    unsafe {
        match step {
            0 => {}
            1 => { LANG = 0; }
            2 => {
                for i in 0..MODULES.len() {
                    MODULE_ON[i] = MODULES[i].default_on;
                }
            }
            3 => { REGION = 0; TIMEZONE = 0; }
            4 => { NET_SKIP = false; }
            5 => { AGREEMENT = false; }
            6 => { CLOUD_ENABLED = false; CLOUD_CONTROL = false; PARENTAL = false; }
            7 => { SEC_TYPE = 0; SEC_ENABLED = false; }
            // 注意：密码/胁迫密码不预填（用户输入），
            // 这里不能重置 —— apply_step_defaults 在 save_step 之前执行，
            // 重置会覆盖用户刚输入的内容导致保存错误的哈希。
            // 空闲自动推进到达 8/9 时输入必为空：关闭 SEC_ENABLED，
            // 让 save_password 走「未启用安全」路径（清空 security_type），
            // 不留下「有 security_type 而无 security_hash」的锁死配置。
            8 | 9 => { SEC_ENABLED = false; }
            10 => { DURESS_ACTION = 0; }
            11 => { THEME_DARK = false; WALLPAPER = 1; NAV_MODE = 0; }
            _ => {}
        }
    }
}

/// 模拟点击（固定坐标）：无输入设备时（如 run-gui）主流程回退调用本函数
/// 应用该步默认选择并自动推进，与 launcher 的模拟点击 fallback 一致。
/// 坐标取底部「下一步 / 进入系统」按钮组件命中区的中心（render 后 rect 有效）。
fn simulate_click(step: usize) {
    let (x, y) = unsafe {
        let b: &button::Button = if STEP < 12 { &BTN_NEXT } else { &BTN_ENTER };
        (b.x + b.w / 2, b.y + b.h / 2)
    };
    print("[oobe] 模拟点击下一步 (");
    print_dec(x as usize);
    print(",");
    print_dec(y as usize);
    print(")
");
    // 各步在点击「下一步」前的默认选择处理
    unsafe {
        match step {
            0 => {}
            1 => { LANG = 0; } // 默认简体中文
            2 => {
                for i in 0..MODULES.len() {
                    MODULE_ON[i] = MODULES[i].default_on;
                }
            }
            3 => { REGION = 0; TIMEZONE = 0; }
            4 => { NET_SKIP = false; }
            5 => { AGREEMENT = false; }
            6 => { CLOUD_ENABLED = false; CLOUD_CONTROL = false; PARENTAL = false; }
            7 => { SEC_TYPE = 0; SEC_ENABLED = false; }
            8 | 9 => {} // 密码/胁迫密码不预填（仅模拟模式下留空推进）
            10 => { DURESS_ACTION = 0; }
            11 => { THEME_DARK = false; WALLPAPER = 1; NAV_MODE = 0; }
            _ => {}
        }
    }
}

// ==================== 主流程 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    // 启动顺序调整：首帧渲染前**同步加载**向导超小子集（/fonts/oobe-cn.ttf
    // 正文 + oobe-bold.ttf 标题，~380KB/867 字形，配合大块缓冲 brk 分配器
    // TCG 慢宿主也秒级）——首帧即平滑 TTF，向导全程无点阵兜底闪现。
    print("[oobe] 正在启动\n");
    mkdir_all();

    // 打开 /dev/fb0 并 mmap 到用户空间，直接绘制像素。
    if !fb_init() {
        print("[oobe] /dev/fb0 mmap 失败，无帧缓冲
");
        unsafe { exit(1); }
    }
    print("[oobe] 帧缓冲 ");
    print_dec(unsafe { FB_W });
    print("x");
    print_dec(unsafe { FB_H });
    print(" 已映射
");

    // 初始化默认模块勾选
    unsafe {
        for i in 0..MODULES.len() {
            MODULE_ON[i] = MODULES[i].default_on;
        }
        STEP = 0;
        // 各输入框默认值（可在输入框中修改）。
        // 密码/胁迫密码不预填：留空由用户输入，避免默认“1234/9999”被直接
        // 当作真实密码保存（校验要求 4-16 位且两次一致，未输入则无法通过）。
        USER_BUF[..4].copy_from_slice(b"user");
        USER_LEN = 4;
        PASS_LEN = 0;
        CONFIRM_LEN = 0;
        DURESS_LEN = 0;
        DURESS_CONFIRM_LEN = 0;
        FIELD_FOCUS = 0;
    }

    let input_fd = unsafe { open(INPUT_PATH, O_RDONLY | O_NONBLOCK) };
    unsafe { INPUT_FD = input_fd; }
    if input_fd < 0 {
        // 无输入设备（如 run-gui 无串口触摸）：回退为固定坐标模拟点击自动推进，
        // 与 launcher 的模拟点击 fallback 一致，避免 OOBE 永久挂起。
        print("[input] /dev/input/event0 未就绪，回退为模拟点击
");
    } else {
        print("[input] /dev/input/event0 就绪
");
    }

    let mut step = 0usize;
    // 无输入设备时用固定坐标模拟点击自动推进（与 launcher 回退一致）。
    let simulated = input_fd < 0;
    // 转场跟踪：from = 上一屏步骤；reverse = 返回导航（新页由上滑入）。
    let mut from = 0usize;
    let mut reverse = false;
    let mut first_show = true;
    while step < 13 {
        unsafe { STEP = step; }
        // 新页面重渲染后，闲置呼吸基线快照需重新抓取。
        idle_snap_invalidate();
        // 切步骤：复位按钮按压/回弹状态，避免上一屏状态残留到新步骤。
        btn_reset_all();
        // 进入输入框步骤时默认聚焦第一个输入框。
        if step == 6 || step == 8 || step == 9 {
            unsafe { FIELD_FOCUS = 0; }
        }
        log_step(step);
        if step == 4 {
            query_network();
        }
        if first_show {
            // 首次进入欢迎页：先同步加载 OOBE 超小子集（首帧即平滑 TTF，
            // 全程无点阵闪现），再渲染首帧并触发启动画面淡出、播放入场动画。
            load_fonts_sync();
            render_and_flip();
            splash_fade();
            animate_welcome();
            first_show = false;
        } else {
            // 其余步骤：ColorOS 15 交叉淡化 + 上滑转场。
            transition_pages(from, step, reverse);
        }
        if step == 12 {
            // 完成页：真实输入时等待用户点击「进入系统」（BTN_ENTER，
            // handle_tap 已接线）；30 秒无输入自动进入，保证无人值守可推进。
            // 「上一步」按钮在本页仍绘制（STEP>0 恒可命中），返回 -1 必须
            // 回退到上一步 —— 此前返回值被忽略，点「上一步」照样进桌面。
            if !simulated && wait_for_tap_next_button() < 0 {
                from = step;
                reverse = true;
                step = 11;
                continue;
            }
            // 进入系统（预加载桌面资源 -> 写 .done -> 退出）。
            print("[oobe] 正在保存配置
");
            finish::enter_desktop();
            print("[oobe] 正在进入桌面
");
            break;
        }

        // 仅「30 秒空闲自动推进」应用该步默认选择；用户点击推进时保留其选择
        // （此前无条件 apply_step_defaults 会把用户刚选的语言/模块/主题/锁屏
        // 方式等全部重置为默认值再保存，用户选择静默丢失）。
        let mut use_defaults = false;
        if simulated {
            // 无输入设备：模拟点击「下一步」（应用该步默认选择），短暂停留后推进。
            simulate_click(step);
            sleep_ms(300);
        } else {
            // 等待用户点击「下一步」（真实输入，不自动跳过）
            let action = wait_for_tap_next_button();
            if action == 3 {
                // 跳过：放弃当前步（不保存），直接进入完成页。
                print("[oobe] 跳过当前步骤，直达完成页
");
                unsafe {
                    // 锁屏密码步骤被跳过时不得留下「有 security_type 但无
                    // security_hash」的配置 —— launcher 会开机即锁死且无法
                    // 解锁。清空安全配置回到默认滑动解锁。
                    if step == 8 {
                        SEC_ENABLED = false;
                        write_file("/data/oobe/security_type", b"");
                    }
                }
                from = step;
                reverse = false;
                step = 12;
                continue;
            }
            if action < 0 {
                reverse = true;
                from = step;
                if step > 0 {
                    step -= 1;
                }
                continue;
            }
            use_defaults = action == 4;
        }

        if use_defaults {
            apply_step_defaults(step);
        }
        save_step(step);

        from = step;
        reverse = false;
        step += 1;
    }

    print("[oobe] 正在退出
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
    unsafe {
        write_fd(2, b"PANIC: oobe ");
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
