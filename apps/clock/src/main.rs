// KunxOS 时钟应用（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由 launcher fork+execve 拉起（/bin/clock）。四个页签 + 闹钟编辑子页 + 响铃叠层：
//
//   「时钟」(世界时钟)
//     顶部数字钟：本地时间（读 /data/oobe/timezone 指定时区，默认 Asia/Shanghai）
//     + 日期星期；下方内置时区表（shared/tz.rs）：北京/东京/首尔/新加坡/迪拜/
//     孟买/伦敦/巴黎/莫斯科/纽约/芝加哥/洛杉矶/圣保罗/悉尼/UTC，实时显示各城市
//     本地时间与 UTC 偏移。
//
//   「闹钟」
//     列表（开关 / 删除 / 点击编辑）+ 新建按钮；编辑子页：时/分步进器、周重复
//     选择（周一~周日 / 每天 / 仅一次）、标签输入（物理键盘）。保存后写
//     /data/alarms.txt 并给 alarmd 发 RELOAD。响铃叠层（alarmd 置 RINGING 状态
//     时）提供「贪睡 5 分」与「停止」。
//
//   「计时」倒计时：预设/±分钟、开始/暂停/继续/重置；到点蜂鸣（audiod）。
//   「秒表」：开始/暂停、计次（圈数 ≤8）、重置；计次按累计+单圈展示。
//
// 关键问题应答（与 alarmd 保持一致）：
//   内核提供 PL031 RTC（QEMU virt），gettimeofday 返回 UTC 纪元时间；无 RTC
//   平台退化为单调时钟。QEMU 无系统休眠（idle=wfi，定时器保持唤醒），闹钟由
//   alarmd 后台守护轮询触发，无需唤醒机制。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::global_asm;
use core::panic::PanicInfo;

#[path = "../../../shared/cjk_font_clock.rs"]
mod cjk_font;

#[path = "../../../shared/app_kit.rs"]
mod kit;

use kit::*;

#[path = "../../../shared/tz.rs"]
mod tz;

#[path = "../../../shared/audio.rs"]
mod audio;

// ==================== 配色（浅色主题） ====================

/// 页面基底背景（浅色渐变：顶部白 → 底部极浅灰）。
const C_BG: u32 = 0x00_F7_F8_FA;
/// 卡片/面板填充（白）。
const C_CARD: u32 = 0x00_FF_FF_FF;
/// 卡片/按钮描边。
const C_CARD_BORDER: u32 = 0x00_D9_DE_E6;
/// 主文字（白底/浅玻璃上）。
const C_TEXT: u32 = 0x00_1A_1C_22;
/// 次要/弱化文字。
const C_DIM: u32 = 0x00_6B_72_80;
const C_ACC: u32 = 0x00_00_B9_6B;
const C_RED: u32 = 0x00_E7_4C_3C;
const C_WARN: u32 = 0x00_F9_73_16;
const C_SWITCH_ON: u32 = 0x00_00_B9_6B;
/// 开关未激活轨道（极浅灰）。
const C_SWITCH_OFF: u32 = 0x00_E9_E9_EA;
/// 选中页签（分段控件选中段白底）。
const C_TAB_ON: u32 = 0x00_FF_FF_FF;
const C_CHIP_ON: u32 = 0x00_00_B9_6B;
/// 分割线。
const C_DIV: u32 = 0x00_E5_E9_F0;
/// 彩色表面（强调绿/危险红）上的文字保持白色。
const C_ON_ACCENT: u32 = 0x00_FF_FF_FF;

// ==================== 协议路径 ====================

const ALARMS_PATH: &str = "/data/alarms.txt";
const STATE_PATH: &str = "/var/run/alarmd.state";
const CTL_PATH: &str = "/var/run/alarmd.ctl";

/// 应用内闹钟上限（与 alarmd 一致）。
const MAX_ALARMS: usize = 16;

// ==================== 页签与子页 ====================

const TAB_WORLD: u8 = 0;
const TAB_ALARM: u8 = 1;
const TAB_TIMER: u8 = 2;
const TAB_STOPWATCH: u8 = 3;

const APAGE_LIST: u8 = 0;
const APAGE_EDIT: u8 = 1;

// ==================== 闹钟数据结构 ====================

#[derive(Clone, Copy)]
struct Alarm {
    id: u32,
    on: bool,
    hh: u32,
    mm: u32,
    /// bit0=周一 … bit6=周日；0 = 一次性。
    repeat: u8,
    label: [u8; 32],
    label_len: usize,
}

impl Alarm {
    const fn empty() -> Alarm {
        Alarm {
            id: 0,
            on: false,
            hh: 8,
            mm: 0,
            repeat: 0,
            label: [0; 32],
            label_len: 0,
        }
    }
}

// ==================== 全局状态 ====================

static mut TAB: u8 = TAB_WORLD;
static mut SCROLL: i32 = 0;

static mut ALARMS: [Alarm; MAX_ALARMS] = [Alarm::empty(); MAX_ALARMS];
static mut ALARM_COUNT: usize = 0;

static mut APAGE: u8 = APAGE_LIST;
static mut EDIT_ID: u32 = 0;
static mut EDIT_NEW: bool = false;
static mut EDIT_HH: u32 = 8;
static mut EDIT_MM: u32 = 0;
static mut EDIT_REP: u8 = 0x7F;
static mut EDIT_LABEL: [u8; 32] = [0; 32];
static mut EDIT_LABEL_LEN: usize = 0;
static mut EDIT_FOCUS: u8 = 0; // 0=无, 1=标签输入
static mut EDIT_DIRTY: bool = false;

/// 计时器：0=空闲 1=运行 2=暂停 3=结束。
static mut TMR_STATE: u8 = 0;
static mut TMR_TOTAL: u64 = 300;
static mut TMR_START_EP: u64 = 0;
static mut TMR_REMAIN: u64 = 300;
static mut TMR_MIN: u32 = 5;

/// 秒表。
static mut SW_RUN: bool = false;
static mut SW_ACC_US: u64 = 0;
static mut SW_RESUME_EP: u64 = 0;
static mut SW_LAPS: usize = 0;
/// 单圈耗时（ms）。
static mut SW_LAP_MS: [u32; 10] = [0; 10];
/// 该圈累计（ms）。
static mut SW_LAP_TOTAL: [u32; 10] = [0; 10];

/// alarmd 状态（0=空闲 1=响铃 2=贪睡中）。
static mut ASTATE: u8 = 0;
static mut ARING_ID: u32 = 0;
static mut ARING_EPOCH: u64 = 0;

static mut ZONE: usize = 0;
static mut ZONE_READ: bool = false;

// ==================== 系统调用与文件（复用 app_kit 提供者） ====================

const O_CREAT: usize = 0x40;
const O_TRUNC: usize = 0x200;
const O_APPEND: usize = 0x400;

fn sys_open(path: &str, flags: usize) -> isize {
    unsafe { open(path, flags) }
}

fn read_file(path: &str, out: &mut [u8]) -> usize {
    let fd = sys_open(path, O_RDONLY);
    if fd < 0 {
        return 0;
    }
    let r = unsafe { syscall3(SYS_READ, fd as usize, out.as_mut_ptr() as usize, out.len()) };
    unsafe { syscall1(SYS_CLOSE, fd as usize) };
    if (r as isize) <= 0 {
        0
    } else {
        (r as usize).min(out.len())
    }
}

fn write_file(path: &str, data: &[u8]) -> bool {
    let fd = sys_open(path, O_WRONLY | O_CREAT | O_TRUNC);
    if fd < 0 {
        return false;
    }
    let r = unsafe { syscall3(SYS_WRITE, fd as usize, data.as_ptr() as usize, data.len()) };
    unsafe { syscall1(SYS_CLOSE, fd as usize) };
    (r as isize) == data.len() as isize
}

// ==================== 十进制与解析 ====================

fn push_dec(out: &mut [u8], n: &mut usize, mut v: u64) {
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    if v == 0 {
        if *n < out.len() {
            out[*n] = b'0';
            *n += 1;
        }
        return;
    }
    while v > 0 {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    let d = tmp.len() - i;
    if *n + d <= out.len() {
        out[*n..*n + d].copy_from_slice(&tmp[i..]);
        *n += d;
    }
}

fn parse_dec(s: &[u8]) -> (u64, usize) {
    let mut i = 0usize;
    let mut v: u64 = 0;
    let mut any = false;
    while i < s.len() && s[i] >= b'0' && s[i] <= b'9' {
        v = v.wrapping_mul(10).wrapping_add((s[i] - b'0') as u64);
        any = true;
        i += 1;
    }
    if any { (v, i) } else { (0, 0) }
}

// ==================== 闹钟列表读写 ====================

/// 解析一行：`id on hh mm repeat label...`。
fn parse_alarm_line(line: &[u8]) -> Option<Alarm> {
    let mut f: [&[u8]; 5] = [&[]; 5];
    let mut n = 0usize;
    let mut i = 0usize;
    while i < line.len() && n < 5 {
        while i < line.len() && (line[i] == b' ' || line[i] == b'\t') {
            i += 1;
        }
        if i >= line.len() {
            break;
        }
        let start = i;
        while i < line.len() && line[i] != b' ' && line[i] != b'\t' {
            i += 1;
        }
        f[n] = &line[start..i];
        n += 1;
    }
    if n < 5 {
        return None;
    }
    let (id, _) = parse_dec(f[0]);
    let (on, _) = parse_dec(f[1]);
    let (hh, _) = parse_dec(f[2]);
    let (mm, _) = parse_dec(f[3]);
    let (rep, _) = parse_dec(f[4]);
    if id == 0 || hh > 23 || mm > 59 {
        return None;
    }
    let mut a = Alarm::empty();
    a.id = id as u32;
    a.on = on != 0;
    a.hh = hh as u32;
    a.mm = mm as u32;
    a.repeat = (rep & 0x7F) as u8;
    let mut li = i;
    while li < line.len() && (line[li] == b' ' || line[li] == b'\t') {
        li += 1;
    }
    let mut end = line.len();
    while end > li && (line[end - 1] == b'\r' || line[end - 1] == b'\n' || line[end - 1] == b' ') {
        end -= 1;
    }
    let k = (end - li).min(32);
    a.label[..k].copy_from_slice(&line[li..li + k]);
    a.label_len = k;
    Some(a)
}

fn load_alarms() {
    let mut buf = [0u8; 2048];
    let got = read_file(ALARMS_PATH, &mut buf);
    let data = &buf[..got];
    let mut count = 0usize;
    let mut i = 0usize;
    while i < data.len() && count < MAX_ALARMS {
        let mut j = i;
        while j < data.len() && data[j] != b'\n' {
            j += 1;
        }
        if let Some(a) = parse_alarm_line(&data[i..j]) {
            unsafe { ALARMS[count] = a; }
            count += 1;
        }
        i = j + 1;
    }
    unsafe { ALARM_COUNT = count };
}

fn persist_alarms() {
    let mut line = [0u8; 96];
    let mut out = [0u8; 2048];
    let mut n = 0usize;
    for i in 0..unsafe { ALARM_COUNT } {
        let a = unsafe { &ALARMS[i] };
        let mut m = 0usize;
        push_dec(&mut line, &mut m, a.id as u64);
        line[m] = b' ';
        m += 1;
        push_dec(&mut line, &mut m, if a.on { 1 } else { 0 });
        line[m] = b' ';
        m += 1;
        push_dec(&mut line, &mut m, a.hh as u64);
        line[m] = b' ';
        m += 1;
        push_dec(&mut line, &mut m, a.mm as u64);
        line[m] = b' ';
        m += 1;
        push_dec(&mut line, &mut m, a.repeat as u64);
        line[m] = b' ';
        m += 1;
        let k = a.label_len.min(32);
        line[m..m + k].copy_from_slice(&a.label[..k]);
        m += k;
        line[m] = b'\n';
        m += 1;
        if n + m > out.len() {
            break;
        }
        out[n..n + m].copy_from_slice(&line[..m]);
        n += m;
    }
    write_file(ALARMS_PATH, &out[..n]);
}

fn next_alarm_id() -> u32 {
    let mut best = 0u32;
    for i in 0..unsafe { ALARM_COUNT } {
        if unsafe { ALARMS[i] }.id > best {
            best = unsafe { ALARMS[i] }.id;
        }
    }
    best + 1
}

/// 向 alarmd 发送一行命令。
fn alarmd_cmd(cmd: &[u8]) {
    let fd = sys_open(CTL_PATH, O_WRONLY | O_CREAT | O_APPEND);
    if fd < 0 {
        return;
    }
    unsafe {
        syscall3(SYS_WRITE, fd as usize, cmd.as_ptr() as usize, cmd.len());
        syscall1(SYS_CLOSE, fd as usize);
    }
}

fn alarmd_cmd_id(cmd: &[u8], id: u32) {
    let mut buf = [0u8; 32];
    buf[..cmd.len()].copy_from_slice(cmd);
    let mut n = cmd.len();
    buf[n] = b' ';
    n += 1;
    push_dec(&mut buf, &mut n, id as u64);
    buf[n] = b'\n';
    n += 1;
    alarmd_cmd(&buf[..n]);
}

fn alarmd_reload() {
    alarmd_cmd(b"RELOAD\n");
}

// ==================== alarmd 状态 ====================

fn read_alarmd_state() {
    let mut buf = [0u8; 64];
    let got = read_file(STATE_PATH, &mut buf);
    let s = &buf[..got];
    unsafe {
        ASTATE = 0;
        ARING_ID = 0;
        ARING_EPOCH = 0;
    }
    if s.starts_with(b"RINGING") {
        unsafe { ASTATE = 1 };
        parse_state_ids(&s[7..]);
    } else if s.starts_with(b"SNOOZED") {
        unsafe { ASTATE = 2 };
    }
}

fn parse_state_ids(s: &[u8]) {
    let (id, c) = parse_dec(s);
    if c == 0 {
        return;
    }
    unsafe { ARING_ID = id as u32 };
    let rest = &s[c..];
    let (ep, _) = parse_dec(trim_leading(rest));
    unsafe { ARING_EPOCH = ep };
}

fn trim_leading(s: &[u8]) -> &[u8] {
    let mut i = 0;
    while i < s.len() && (s[i] == b' ' || s[i] == b'\t') {
        i += 1;
    }
    &s[i..]
}

// ==================== 时区 ====================

fn current_zone() -> usize {
    unsafe {
        if ZONE_READ {
            return ZONE;
        }
        let mut buf = [0u8; 64];
        let got = read_file("/data/oobe/timezone", &mut buf);
        let mut end = 0usize;
        while end < got && buf[end] != b'\n' && buf[end] != b'\r' && buf[end] != b' ' {
            end += 1;
        }
        let name = core::str::from_utf8(&buf[..end]).unwrap_or("");
        ZONE = tz::zone_by_name(name).unwrap_or(0);
        ZONE_READ = true;
        ZONE
    }
}

fn local_now() -> Option<tz::LocalTime> {
    let ep = now_us() / 1_000_000;
    tz::epoch_to_local(current_zone(), ep as i64)
}

// ==================== 绘制辅助 ====================

/// BACK_BUF 抗锯齿绘制目标（clock 经 app_kit 共享帧缓冲）。
struct ClockSmooth;

impl glass::SmoothTarget for ClockSmooth {
    fn put_aa(&mut self, x: i32, y: i32, color: u32, alpha: u32) {
        blend_put(x, y, color, alpha);
    }
}

/// 填充圆角矩形（抗锯齿超椭圆连续曲率，ColorOS 15 观感）。
fn fill_round_rect(x: i32, y: i32, w: i32, h: i32, r: i32, color: u32) {
    glass::smooth_fill_rr(&mut ClockSmooth, x, y, w, h, r, color);
}

/// 实心圆（抗锯齿）。
fn fill_circle(cx: i32, cy: i32, r: i32, color: u32) {
    glass::smooth_fill_circle(&mut ClockSmooth, cx, cy, r, color);
}

fn draw_bits(x: i32, y: i32, bits: &[u8; 8], scale: i32, color: u32) {
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

fn ttf_glyph_px(x: i32, y: i32, cp: u32, px: f32, color: u32) -> i32 {
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

fn text_top(px: f32, cy: i32) -> i32 {
    if !ttf_text::font_render::font_ready() {
        return cy - (px as i32 / 2);
    }
    let (asc, desc) = ttf_text::font_render::line_metrics(px);
    cy - ((asc - desc) as i32)
}

fn draw_center_text(y: i32, s: &[u8], px: f32, color: u32) {
    let tx = (DISPLAY_W as i32 - text_width_px(s, px)) / 2;
    draw_text_px(tx, y, s, px, color);
}

/// 8x8 数字字形（scale 放大）。
fn draw_sdigits(x: i32, y: i32, s: &[u8], scale: i32, color: u32) -> i32 {
    let mut cx = x;
    for &c in s {
        let g = glyph(c);
        for (row, bits) in g.iter().enumerate() {
            for col in 0..8 {
                if bits & (0x80 >> col) != 0 {
                    for dy in 0..scale {
                        for dx in 0..scale {
                            put_pixel(cx + col as i32 * scale + dx, y + row as i32 * scale + dy, color);
                        }
                    }
                }
            }
        }
        cx += 8 * scale;
    }
    cx - x
}

fn draw_center_digits(y: i32, s: &[u8], scale: i32, color: u32) {
    let w = (s.len() as i32) * 8 * scale;
    draw_sdigits((DISPLAY_W as i32 - w) / 2, y, s, scale, color);
}

// ==================== 选项卡 ====================

const TAB_Y: i32 = 44;
const TAB_H: i32 = 44;

// ==================== 标签切换动画 ====================
// 规格对齐 docs/animations.md：指示药丸从旧标签位 ease-out 滑到新标签位
// （180ms，Material FastOutSlowIn 节奏）；文字选中态随逻辑标签立即切换。

static mut TAB_PREV: u8 = 0;
/// 动画起始时刻（µs，0 = 空闲）。
static mut TAB_ANIM_START: u64 = 0;
const TAB_ANIM_MS: u64 = 180;

/// ease-out（0..255 → 0..255，二次曲线近似，与 launcher 运动库同形）。
fn ease_out8(t: u32) -> u32 {
    let inv = 255 - t.min(255);
    255 - (inv * inv + 127) / 255
}

/// 指示药丸的动画 x 起点（像素）。动画未激活时随逻辑标签。
fn tab_pill_x(w: i32) -> i32 {
    unsafe {
        let now = now_us();
        let el = now.saturating_sub(TAB_ANIM_START) / 1000;
        if TAB_ANIM_START != 0 && el < TAB_ANIM_MS {
            let t = ((el * 255) / TAB_ANIM_MS) as u32;
            let e = ease_out8(t) as i32;
            let from = TAB_PREV as i32 * w;
            let to = TAB as i32 * w;
            from + (to - from) * e / 255
        } else {
            TAB as i32 * w
        }
    }
}

/// 标签切换动画是否在播（到期帧返回 true 一次，供恢复常态重绘）。
fn tab_anim_active() -> bool {
    unsafe {
        if TAB_ANIM_START != 0 {
            if now_us().saturating_sub(TAB_ANIM_START) / 1000 < TAB_ANIM_MS {
                return true;
            }
            TAB_ANIM_START = 0;
        }
    }
    false
}

fn draw_tabs() {
    let labels: [&[u8]; 4] = [
        "时钟".as_bytes(),
        "闹钟".as_bytes(),
        "计时".as_bytes(),
        "秒表".as_bytes(),
    ];
    let w = DISPLAY_W as i32 / 4;
    let px = tab_pill_x(w);
    for i in 0..4 {
        let x = i as i32 * w;
        let on = unsafe { TAB } == i as u8;
        if on {
            fill_round_rect(px + 6, TAB_Y + 6, w - 12, TAB_H - 10, 12, C_TAB_ON);
        }
        draw_center_text(TAB_Y + 13, labels[i], 15.0, if on { C_TEXT } else { C_DIM });
        if i < 3 {
            fill_rect(x + w - 1, TAB_Y + 10, 1, TAB_H - 20, C_DIV);
        }
    }
    fill_rect(0, TAB_Y + TAB_H - 1, DISPLAY_W as i32, 1, C_DIV);
}

// ==================== 世界时钟 ====================

const CITIES: [(usize, &[u8]); 15] = [
    (0, "北京".as_bytes()),
    (1, "东京".as_bytes()),
    (2, "首尔".as_bytes()),
    (3, "新加坡".as_bytes()),
    (4, "迪拜".as_bytes()),
    (5, "孟买".as_bytes()),
    (6, "伦敦".as_bytes()),
    (7, "巴黎".as_bytes()),
    (8, "莫斯科".as_bytes()),
    (9, "纽约".as_bytes()),
    (10, "芝加哥".as_bytes()),
    (11, "洛杉矶".as_bytes()),
    (12, "圣保罗".as_bytes()),
    (13, "悉尼".as_bytes()),
    (14, "UTC".as_bytes()),
];

fn draw_world() {
    // 顶部数字钟：HH:MM:SS。
    let us = now_us();
    let t = if us != 0 { (us / 1_000_000) as u64 } else { 0 };
    let lt = tz::epoch_to_local(current_zone(), t as i64);
    let mut clock = [0u8; 8];
    if let Some(l) = lt {
        clock[0] = b'0' + (l.h / 10) as u8;
        clock[1] = b'0' + (l.h % 10) as u8;
        clock[2] = b':';
        clock[3] = b'0' + (l.min / 10) as u8;
        clock[4] = b'0' + (l.min % 10) as u8;
        clock[5] = b':';
        clock[6] = b'0' + (l.s / 10) as u8;
        clock[7] = b'0' + (l.s % 10) as u8;
        draw_center_digits(100, &clock, 4, C_TEXT);
        // 日期 + 星期。
        let wd = weekday_from_days(t as i64 / 86400);
        let mut date = [0u8; 32];
        let mut n = 0usize;
        push_dec(&mut date, &mut n, l.y as u64);
        date[n] = b'-';
        n += 1;
        let _ = push_2d(&mut date, &mut n, l.mo);
        date[n] = b'-';
        n += 1;
        let _ = push_2d(&mut date, &mut n, l.d);
        date[n] = b' ';
        n += 1;
        let lbl = weekday_label(wd);
        if n + lbl.len() <= date.len() {
            date[n..n + lbl.len()].copy_from_slice(lbl);
        }
        n += lbl.len();
        draw_center_text(164, &date[..n], 16.0, C_DIM);
    } else {
        draw_center_digits(100, b"--:--:--", 4, C_DIM);
    }

    // 城市列表。
    let row_h = 62;
    let top = 200;
    let bottom = content_bottom();
    let n = CITIES.len() as i32;
    let list_h = n * row_h;
    let scroll = unsafe { SCROLL }.min(list_h - (bottom - top));
    let scroll = scroll.max(0);
    unsafe { SCROLL = scroll };
    let zone = current_zone();
    for i in 0..CITIES.len() {
        let y = top - scroll + i as i32 * row_h;
        if y + row_h <= top || y >= bottom {
            continue;
        }
        let c = CITIES[i];
        let is_home = c.0 == zone;
        fill_round_rect(12, y + 4, DISPLAY_W as i32 - 24, row_h - 8, 12, C_CARD);
        // 城市名。
        draw_text_px(26, text_top(16.0, y + row_h / 2), c.1, 16.0, C_TEXT);
        // 本地时间。
        let lt2 = tz::epoch_to_local(c.0, t as i64);
        if let Some(l2) = lt2 {
            let mut ts = [0u8; 6];
            ts[0] = b'0' + (l2.h / 10) as u8;
            ts[1] = b'0' + (l2.h % 10) as u8;
            ts[2] = b':';
            ts[3] = b'0' + (l2.min / 10) as u8;
            ts[4] = b'0' + (l2.min % 10) as u8;
            // HH:MM 共 5 字节（ts[5] 不绘制；此前误写分钟个位两遍，
            // 城市行时间显示成「00:488」）。
            let w = text_width_px(&ts[..5], 22.0);
            draw_text_px(DISPLAY_W as i32 - 30 - w, text_top(22.0, y + row_h / 2), &ts[..5], 22.0, if is_home { C_ACC } else { C_TEXT });
            // 偏移。
            let off = l2.offset_min;
            let mut ob = [0u8; 10];
            let mut on_ = 0usize;
            ob[on_] = if off < 0 { b'-' } else { b'+' };
            on_ += 1;
            let aoff = if off < 0 { -off } else { off } as u32;
            let _ = push_2d(&mut ob, &mut on_, aoff / 60);
            ob[on_] = b':';
            on_ += 1;
            let _ = push_2d(&mut ob, &mut on_, aoff % 60);
            if l2.dst {
                if on_ + 4 <= ob.len() {
                    ob[on_..on_ + 4].copy_from_slice(b" DST");
                    on_ += 4;
                }
            }
            draw_text_px(26, y + row_h - 20, &ob[..on_], 11.0, C_DIM);
        }
        if is_home {
            // 「本地」角标。
            let lbl = "本地".as_bytes();
            let w = text_width_px(lbl, 12.0);
            draw_text_px(DISPLAY_W as i32 - 26 - w, text_top(12.0, y + row_h / 2) - 24, lbl, 12.0, C_ACC);
        }
    }
}

fn push_2d(out: &mut [u8], n: &mut usize, v: u32) -> usize {
    if *n + 1 < out.len() {
        out[*n] = b'0' + (v / 10) as u8;
        out[*n + 1] = b'0' + (v % 10) as u8;
        *n += 2;
    }
    *n
}

fn weekday_label(wd: u32) -> &'static [u8] {
    match wd {
        0 => "星期日".as_bytes(),
        1 => "星期一".as_bytes(),
        2 => "星期二".as_bytes(),
        3 => "星期三".as_bytes(),
        4 => "星期四".as_bytes(),
        5 => "星期五".as_bytes(),
        _ => "星期六".as_bytes(),
    }
}

// ==================== 闹钟列表 ====================

fn draw_switch(x: i32, y: i32, on: bool) {
    const SW_W: i32 = 48;
    const SW_H: i32 = 28;
    fill_round_rect(x, y, SW_W, SW_H, 14, if on { C_SWITCH_ON } else { C_SWITCH_OFF });
    let kx = if on { x + SW_W - 24 - 2 } else { x + 2 };
    // 开关旋钮保持白色（浅色轨道上仍可辨识）。
    fill_circle(kx + 12, y + SW_H / 2, 11, C_ON_ACCENT);
}

fn repeat_text(rep: u8, out: &mut [u8]) -> usize {
    if rep == 0 {
        let s = "仅一次".as_bytes();
        let k = s.len().min(out.len());
        out[..k].copy_from_slice(&s[..k]);
        return k;
    }
    if rep == 0x7F {
        let s = "每天".as_bytes();
        let k = s.len().min(out.len());
        out[..k].copy_from_slice(&s[..k]);
        return k;
    }
    if rep == 0x1F {
        let s = "工作日".as_bytes();
        let k = s.len().min(out.len());
        out[..k].copy_from_slice(&s[..k]);
        return k;
    }
    if rep == 0x60 {
        let s = "周末".as_bytes();
        let k = s.len().min(out.len());
        out[..k].copy_from_slice(&s[..k]);
        return k;
    }
    const DAYS: [&[u8]; 7] = [
        "周一".as_bytes(),
        "周二".as_bytes(),
        "周三".as_bytes(),
        "周四".as_bytes(),
        "周五".as_bytes(),
        "周六".as_bytes(),
        "周日".as_bytes(),
    ];
    let mut n = 0usize;
    for i in 0..7 {
        if (rep >> i) & 1 != 0 {
            let mut single = [0u8; 6];
            let k = (DAYS[i].len()).min(6);
            single[..k].copy_from_slice(&DAYS[i][..k]);
            if n + k + 1 <= out.len() {
                out[n..n + k].copy_from_slice(&single[..k]);
                n += k;
                if n < out.len() {
                    out[n] = b'/';
                    n += 1;
                }
            }
        }
    }
    if n > 0 {
        n -= 1; // 去掉末尾 /
    }
    n
}

fn draw_alarm_list() {
    let top = 92;
    let bottom = content_bottom();
    let n = unsafe { ALARM_COUNT } as i32;
    let row_h = 92;
    if n == 0 {
        draw_center_text((top + bottom) / 2 - 30, "暂无闹钟，点右下角新建".as_bytes(), 16.0, C_DIM);
    }
    let list_h = n * row_h;
    let scroll = unsafe { SCROLL }.min((list_h - (bottom - top)).max(0));
    let scroll = scroll.max(0);
    unsafe { SCROLL = scroll };
    for i in 0..unsafe { ALARM_COUNT } {
        let y = top - scroll + i as i32 * row_h;
        if y + row_h <= top || y >= bottom {
            continue;
        }
        let a = unsafe { ALARMS[i] };
        let dim = if a.on { C_TEXT } else { C_DIM };
        fill_round_rect(12, y + 4, DISPLAY_W as i32 - 24, row_h - 8, 14, C_CARD);
        // 时间。
        let mut ts = [0u8; 5];
        ts[0] = b'0' + (a.hh / 10) as u8;
        ts[1] = b'0' + (a.hh % 10) as u8;
        ts[2] = b':';
        ts[3] = b'0' + (a.mm / 10) as u8;
        ts[4] = b'0' + (a.mm % 10) as u8;
        draw_text_px(28, text_top(30.0, y + row_h / 2), &ts, 30.0, dim);
        // 重复 + 标签。
        let mut sub = [0u8; 48];
        let mut sn = repeat_text(a.repeat, &mut sub);
        if a.label_len > 0 && sn + 1 <= sub.len() {
            sub[sn] = b' ';
            sn += 1;
            let k = a.label_len.min(32);
            if sn + k <= sub.len() {
                sub[sn..sn + k].copy_from_slice(&a.label[..k]);
                sn += k;
            }
        }
        if sn > 0 {
            draw_text_px(28, y + row_h - 2, &sub[..sn], 12.0, C_DIM);
        }
        // 开关。
        draw_switch(DISPLAY_W as i32 - 26 - 48, y + (row_h - 28) / 2 - 12, a.on);
        // 删除。
        fill_circle(DISPLAY_W as i32 - 26 - 16, y + row_h - 22, 12, C_RED);
        // 危险红圆盘上的 × 保持白色。
        draw_center_text(y + row_h - 28, "×".as_bytes(), 16.0, C_ON_ACCENT);
    }

    // 新建按钮（右下角悬浮）。
    let add = "＋ 新建".as_bytes();
    let w = text_width_px(add, 15.0);
    fill_round_rect(DISPLAY_W as i32 - w - 42, bottom - 52, w + 30, 40, 20, C_ACC);
    // 强调绿底上保持白字。
    draw_center_text(bottom - 42, add, 15.0, C_ON_ACCENT);
}

// ==================== 闹钟编辑 ====================

fn draw_edit() {
    let _top = 92;
    let bottom = content_bottom();
    // 时间显示。
    let mut ts = [0u8; 5];
    ts[0] = b'0' + (unsafe { EDIT_HH } / 10) as u8;
    ts[1] = b'0' + (unsafe { EDIT_HH } % 10) as u8;
    ts[2] = b':';
    ts[3] = b'0' + (unsafe { EDIT_MM } / 10) as u8;
    ts[4] = b'0' + (unsafe { EDIT_MM } % 10) as u8;
    draw_center_digits(116, &ts, 4, C_TEXT);

    // 小时步进器。
    let col_y = 200;
    draw_plus_minus(90, col_y, true);
    // 分钟步进器。
    draw_plus_minus(380, col_y, false);

    // 周重复。
    let cy = 300;
    draw_text_px(26, text_top(14.0, cy), "重复".as_bytes(), 14.0, C_DIM);
    let chip_w = (DISPLAY_W as i32 - 52) / 7;
    for i in 0..7 {
        let x = 26 + i as i32 * chip_w;
        let on = (unsafe { EDIT_REP } >> i) & 1 != 0;
        fill_round_rect(x, cy + 16, chip_w - 4, 38, 10, if on { C_CHIP_ON } else { C_CARD });
        let d = weekday_char(i);
        let mut b = [0u8; 4];
        let k = d.encode_utf8(&mut b).len();
        // 选中绿 chip 上白字，未选中白底 chip 上深字。
        draw_center_text(cy + 24, &b[..k], 14.0, if on { C_ON_ACCENT } else { C_TEXT });
    }
    // 快速选项。
    draw_quick_chips(cy + 70);
    // 标签。
    let ly = cy + 118;
    draw_text_px(26, text_top(14.0, ly), "标签".as_bytes(), 14.0, C_DIM);
    let focused = unsafe { EDIT_FOCUS } == 1;
    // 标签输入框：聚焦白底、未聚焦极浅灰（浅色输入框规范）。
    fill_round_rect(26, ly + 12, DISPLAY_W as i32 - 52, 42, 12, if focused { C_CARD } else { 0x00_F7_F8_FA });
    let label = unsafe { &EDIT_LABEL[..EDIT_LABEL_LEN] };
    if label.is_empty() {
        draw_text_px(40, text_top(15.0, ly + 33), "输入标签（可选）".as_bytes(), 15.0, C_DIM);
    } else {
        draw_text_px(40, text_top(15.0, ly + 33), label, 15.0, C_TEXT);
        if focused {
            let w = text_width_px(label, 15.0);
            fill_rect(40 + w + 2, ly + 20, 2, 20, C_TEXT);
        }
    }

    // 保存 / 取消。
    let by = bottom - 54;
    draw_text_button(26, by, 180, 44, "取消".as_bytes(), false);
    draw_text_button(DISPLAY_W as i32 - 26 - 180, by, 180, 44, "保存".as_bytes(), true);
}

fn weekday_char(i: u32) -> char {
    match i {
        0 => '一',
        1 => '二',
        2 => '三',
        3 => '四',
        4 => '五',
        5 => '六',
        _ => '日',
    }
}

fn draw_plus_minus(x: i32, y: i32, hour: bool) {
    // 上箭头。
    fill_round_rect(x - 30, y, 60, 40, 14, C_CARD);
    draw_center_text(y + 11, "✚".as_bytes(), 17.0, C_TEXT);
    // 数值标签。
    let lbl: &[u8] = if hour { "小时".as_bytes() } else { "分钟".as_bytes() };
    draw_center_text(y + 56, lbl, 13.0, C_DIM);
    // 下箭头。
    fill_round_rect(x - 30, y + 86, 60, 40, 14, C_CARD);
    draw_center_text(y + 97, "−".as_bytes(), 17.0, C_TEXT);
}

fn draw_quick_chips(y: i32) {
    let labels: [&[u8]; 3] = ["每天".as_bytes(), "工作日".as_bytes(), "仅一次".as_bytes()];
    let cw = (DISPLAY_W as i32 - 52 - 12) / 3;
    for i in 0..3 {
        let x = 26 + i as i32 * (cw + 6);
        let rep = unsafe { EDIT_REP };
        let on = if i == 0 {
            rep == 0x7F
        } else if i == 1 {
            rep == 0x1F
        } else {
            rep == 0
        };
        fill_round_rect(x, y, cw - 6, 36, 10, if on { C_CHIP_ON } else { C_CARD });
        // 选中绿 chip 上白字，未选中白底 chip 上深字。
        draw_center_text(y + 9, labels[i], 13.0, if on { C_ON_ACCENT } else { C_TEXT });
    }
}

fn draw_text_button(x: i32, y: i32, w: i32, h: i32, label: &[u8], primary: bool) {
    if primary {
        fill_round_rect(x, y, w, h, 12, C_ACC);
    } else {
        // 次要按钮：浅灰底（原深色填充浅色化）。
        fill_round_rect(x, y, w, h, 12, 0x00_F2_F4_F8);
    }
    // 主按钮（强调绿）上白字，次要浅灰底上深字。
    draw_center_text(text_top(16.0, y + h / 2), label, 16.0, if primary { C_ON_ACCENT } else { C_TEXT });
}

// ==================== 计时器 ====================

fn timer_remaining() -> u64 {
    unsafe {
        match TMR_STATE {
            1 => {
                let ep = now_us() / 1_000_000;
                TMR_TOTAL.saturating_sub(ep.saturating_sub(TMR_START_EP))
            }
            2 => TMR_REMAIN,
            _ => TMR_TOTAL,
        }
    }
}

fn draw_timer() {
    let _top = 92;
    let bottom = content_bottom();
    let rem = timer_remaining();
    let run = unsafe { TMR_STATE } == 1;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let mut ts = [0u8; 8];
    if h > 0 {
        ts[0] = b'0' + (h / 10) as u8;
        ts[1] = b'0' + (h % 10) as u8;
        ts[2] = b':';
        ts[3] = b'0' + (m / 10) as u8;
        ts[4] = b'0' + (m % 10) as u8;
        ts[5] = b':';
        ts[6] = b'0' + (s / 10) as u8;
        ts[7] = b'0' + (s % 10) as u8;
        draw_center_digits(150, &ts, 4, if run { C_TEXT } else { C_DIM });
    } else {
        ts[0] = b'0' + (m / 10) as u8;
        ts[1] = b'0' + (m % 10) as u8;
        ts[2] = b':';
        ts[3] = b'0' + (s / 10) as u8;
        ts[4] = b'0' + (s % 10) as u8;
        draw_center_digits(170, &ts[..5], 5, if run { C_TEXT } else { C_DIM });
    }

    // 设置分钟。
    let _set_min = unsafe { TMR_MIN };
    let mins = "× 分钟".as_bytes().to_vec();
    let _ = mins;
    // 预设。
    draw_text_px(26, text_top(14.0, 320), "预设".as_bytes(), 14.0, C_DIM);
    let presets: [u32; 6] = [1, 3, 5, 10, 15, 30];
    let pw = (DISPLAY_W as i32 - 52 - 30) / 6;
    for i in 0..6 {
        let x = 26 + i as i32 * (pw + 6);
        let val = presets[i];
        let sel = unsafe { TMR_STATE } == 0 && unsafe { TMR_MIN } == val;
        fill_round_rect(x, 336, pw - 6, 40, 10, if sel { C_CHIP_ON } else { C_CARD });
        let mut b = [0u8; 6];
        let mut bn = 0usize;
        push_dec(&mut b, &mut bn, val as u64);
        // "分"（UTF-8：E5 88 86）。
        let u = [0xE5u8, 0x88, 0x86];
        if bn + 3 <= b.len() {
            b[bn..bn + 3].copy_from_slice(&u);
            bn += 3;
        }
        // 选中绿 chip 上白字，未选中白底 chip 上深字。
        draw_center_text(344, &b[..bn], 15.0, if sel { C_ON_ACCENT } else { C_TEXT });
    }

    // 按钮区。
    let by = bottom - 60;
    match unsafe { TMR_STATE } {
        0 => {
            draw_text_button(30, by, 150, 46, "开始".as_bytes(), true);
            draw_text_button(360, by, 150, 46, "重置".as_bytes(), false);
        }
        1 => {
            draw_text_button(30, by, 150, 46, "暂停".as_bytes(), false);
            draw_text_button(360, by, 150, 46, "重置".as_bytes(), false);
        }
        2 => {
            draw_text_button(30, by, 150, 46, "继续".as_bytes(), true);
            draw_text_button(360, by, 150, 46, "重置".as_bytes(), false);
        }
        _ => {
            draw_center_text(by - 40, "时间到！".as_bytes(), 20.0, C_ACC);
            draw_text_button(30, by, 150, 46, "重置".as_bytes(), false);
            draw_text_button(360, by, 150, 46, "再来".as_bytes(), false);
        }
    }
}

// ==================== 秒表 ====================

fn sw_elapsed_ms() -> u64 {
    unsafe {
        let mut acc = SW_ACC_US;
        if SW_RUN {
            let ep = now_us();
            acc += ep.saturating_sub(SW_RESUME_EP);
        }
        acc / 1000
    }
}

fn draw_stopwatch() {
    let top = 92;
    let bottom = content_bottom();
    let ms = sw_elapsed_ms();
    let cent = (ms % 1000) / 10;
    let tot_s = ms / 1000;
    let (h, m, s) = (tot_s / 3600, (tot_s % 3600) / 60, tot_s % 60);
    let mut ts = [0u8; 10];
    if h > 0 {
        ts[0] = b'0' + (h / 10) as u8;
        ts[1] = b'0' + (h % 10) as u8;
        ts[2] = b':';
        ts[3] = b'0' + (m / 10) as u8;
        ts[4] = b'0' + (m % 10) as u8;
        ts[5] = b':';
        ts[6] = b'0' + (s / 10) as u8;
        ts[7] = b'0' + (s % 10) as u8;
        ts[8] = b'.';
        ts[9] = b'0' + (cent / 10) as u8;
        draw_center_digits(120, &ts, 3, C_TEXT);
    } else {
        ts[0] = b'0' + (m / 10) as u8;
        ts[1] = b'0' + (m % 10) as u8;
        ts[2] = b':';
        ts[3] = b'0' + (s / 10) as u8;
        ts[4] = b'0' + (s % 10) as u8;
        ts[5] = b'.';
        ts[6] = b'0' + (cent / 10) as u8;
        ts[7] = b'0' + (cent % 10) as u8;
        draw_center_digits(140, &ts[..8], 4, C_TEXT);
    }

    // 按钮。
    let by = bottom - 60;
    if unsafe { SW_RUN } {
        draw_text_button(30, by, 150, 46, "暂停".as_bytes(), false);
        draw_text_button(360, by, 150, 46, "计次".as_bytes(), true);
    } else {
        draw_text_button(30, by, 150, 46, "开始".as_bytes(), true);
        if unsafe { SW_LAPS } > 0 {
            draw_text_button(360, by, 150, 46, "重置".as_bytes(), false);
        } else {
            draw_text_button(360, by, 150, 46, "重置".as_bytes(), false);
        }
    }

    // 计次列表。
    let lt = 210;
    let n = unsafe { SW_LAPS };
    for i in (0..n).rev() {
        let idx = n - 1 - i;
        if idx >= 10 {
            continue;
        }
        let y = lt + idx as i32 * 40;
        if y + 40 <= top || y >= bottom - 70 {
            continue;
        }
        let lap = unsafe { SW_LAP_MS[idx] };
        let total = unsafe { SW_LAP_TOTAL[idx] };
        let mut lb = [0u8; 8];
        lb[0] = b'#';
        let mut bn = 1usize;
        push_dec(&mut lb, &mut bn, (idx + 1) as u64);
        draw_text_px(30, text_top(14.0, y + 20), &lb[..bn], 14.0, C_DIM);
        let mut tb = [0u8; 12];
        let mut tn = 0usize;
        push_ms(&mut tb, &mut tn, total as u64);
        draw_text_px(DISPLAY_W as i32 - 30 - text_width_px(&tb[..tn], 15.0), text_top(15.0, y + 20), &tb[..tn], 15.0, C_TEXT);
        let mut lpb = [0u8; 12];
        let mut lp = 0usize;
        push_ms(&mut lpb, &mut lp, lap as u64);
        draw_text_px(DISPLAY_W as i32 - 30 - text_width_px(&lpb[..lp], 13.0), text_top(13.0, y + 40), &lpb[..lp], 13.0, C_DIM);
        fill_rect(30, y + 39, DISPLAY_W as i32 - 60, 1, C_DIV);
    }
}

fn push_ms(out: &mut [u8], n: &mut usize, ms: u64) {
    let cent = (ms % 1000) / 10;
    let tot_s = ms / 1000;
    let (m, s) = (tot_s / 60, tot_s % 60);
    if m > 0 {
        push_dec(out, n, m);
        out[*n] = b':';
        *n += 1;
    }
    let _ = push_2d(out, n, s as u32);
    out[*n] = b'.';
    *n += 1;
    let _ = push_2d(out, n, cent as u32);
}

// ==================== 响铃叠层 ====================

fn draw_ringing_overlay() {
    let top = 92;
    let bottom = content_bottom();
    // 半透明底。
    let cx = DISPLAY_W as i32 / 2;
    let cy = (top + bottom) / 2;
    for dy in -220..220 {
        for dx in -230..230 {
            let px_ = cx + dx;
            let py_ = cy + dy;
            if px_ >= 0 && px_ < DISPLAY_W as i32 && py_ >= top && py_ < bottom {
                let bg = unsafe { BACK_BUF[py_ as usize * DISPLAY_W + px_ as usize] };
                let r = ((bg >> 16) & 0xFF) as u32 * 3 / 4;
                let g = ((bg >> 8) & 0xFF) as u32 * 3 / 4;
                let b = (bg & 0xFF) as u32 * 3 / 4;
                put_pixel(px_, py_, (r << 16) | (g << 8) | b);
            }
        }
    }
    // 卡片。
    fill_round_rect(cx - 220, cy - 180, 440, 360, 20, C_CARD);
    draw_rect(cx - 220, cy - 180, 440, 360, C_CARD_BORDER);
    draw_center_text(cy - 160, "闹钟".as_bytes(), 18.0, C_ACC);
    // 时间。
    let ep = unsafe { ARING_EPOCH };
    let lt = tz::epoch_to_local(current_zone(), ep as i64);
    let mut ts = [0u8; 5];
    if let Some(l) = lt {
        ts[0] = b'0' + (l.h / 10) as u8;
        ts[1] = b'0' + (l.h % 10) as u8;
        ts[2] = b':';
        ts[3] = b'0' + (l.min / 10) as u8;
        ts[4] = b'0' + (l.min % 10) as u8;
        draw_center_digits(cy - 110, &ts, 5, C_TEXT);
    }
    // 标签。
    if let Some(a) = find_alarm(unsafe { ARING_ID }) {
        let label = unsafe { &a.label[..a.label_len] };
        if !label.is_empty() {
            draw_center_text(cy - 20, label, 16.0, C_TEXT);
        }
    }
    let snooze = "贪睡 5 分".as_bytes();
    let stop = "停止".as_bytes();
    let w1 = text_width_px(snooze, 16.0);
    let w2 = text_width_px(stop, 16.0);
    // 贪睡：浅灰底深字；停止：危险红底白字。
    fill_round_rect(cx - 200, cy + 70, 200, 48, 14, 0x00_F2_F4_F8);
    draw_center_text(cy + 82, snooze, 16.0, C_TEXT);
    fill_round_rect(cx, cy + 70, 200, 48, 14, C_RED);
    draw_center_text(cy + 82, stop, 16.0, C_ON_ACCENT);
    let _ = w1;
    let _ = w2;
}

fn find_alarm(id: u32) -> Option<&'static Alarm> {
    for i in 0..unsafe { ALARM_COUNT } {
        if unsafe { ALARMS[i] }.id == id {
            return Some(unsafe { &ALARMS[i] });
        }
    }
    None
}

// ==================== 渲染 ====================

fn render() {
    draw_gradient_bg(C_BG, glass_dark());
    draw_title_bar("时钟".as_bytes());
    draw_tabs();

    read_alarmd_state();

    let ringing = unsafe { ASTATE } == 1;
    if unsafe { APAGE } == APAGE_EDIT && unsafe { TAB } == TAB_ALARM && !ringing {
        draw_edit();
    } else {
        match unsafe { TAB } {
            TAB_WORLD => draw_world(),
            TAB_ALARM => draw_alarm_list(),
            TAB_TIMER => draw_timer(),
            _ => draw_stopwatch(),
        }
    }

    if ringing {
        draw_ringing_overlay();
    }

    draw_sys_nav();
}

// ==================== 交互 ====================

fn tap_world(_x: i32, y: i32) {
    // 城市列表滚动。
    let top = 200;
    let row_h = 62;
    // 滚动条拖动手势略：仅支持点击切换「本地」时区。
    for i in 0..CITIES.len() {
        let yy = top - unsafe { SCROLL } + i as i32 * row_h;
        if y >= yy + 4 && y < yy + row_h {
            let c = CITIES[i];
            if c.0 != current_zone() {
                // 设置本地时区（写入 OOBE 配置，与设置页一致）。
                let name = tz::TZDB[c.0].name.as_bytes();
                write_file("/data/oobe/timezone", name);
                unsafe { ZONE_READ = false };
                // 影响闹钟时区：通知 alarmd 重载（RELOAD 后重新计算）。
                alarmd_reload();
            }
            return;
        }
    }
}

fn tap_alarm_list(x: i32, y: i32) {
    let top = 92;
    let bottom = content_bottom();
    // 新建按钮。
    let add = "＋ 新建".as_bytes();
    let w = text_width_px(add, 15.0);
    let ax = DISPLAY_W as i32 - w - 42;
    let ay = bottom - 52;
    if x >= ax && x < ax + w + 30 && y >= ay && y < ay + 40 {
        unsafe {
            EDIT_ID = next_alarm_id();
            EDIT_NEW = true;
            EDIT_HH = 8;
            EDIT_MM = 0;
            EDIT_REP = 0x7F;
            EDIT_LABEL = [0; 32];
            EDIT_LABEL_LEN = 0;
            EDIT_FOCUS = 0;
            APAGE = APAGE_EDIT;
        }
        return;
    }
    let row_h = 92;
    for i in 0..unsafe { ALARM_COUNT } {
        let yy = top - unsafe { SCROLL } + i as i32 * row_h;
        if y < yy || y >= yy + row_h {
            continue;
        }
        // 开关（x 限定在开关绘制区：此前只判 y，行上半部任意 x 都会翻转
        // 开关并落盘，行内「点击 = 编辑」永远不可达）。
        let sx = DISPLAY_W as i32 - 26 - 48;
        if x >= sx
            && x < sx + 48
            && y < yy + (row_h - 28) / 2 - 12 + 28
            && y >= yy + (row_h - 28) / 2 - 12
        {
            let on = unsafe { ALARMS[i] }.on;
            unsafe { ALARMS[i].on = !on };
            persist_alarms();
            alarmd_reload();
            return;
        }
        // 删除。
        let dx = DISPLAY_W as i32 - 26 - 16;
        let dy = yy + row_h - 22;
        if (x - dx).abs() <= 12 && (y - dy).abs() <= 12 {
            unsafe {
                for k in i..ALARM_COUNT.saturating_sub(1) {
                    ALARMS[k] = ALARMS[k + 1];
                }
                ALARM_COUNT -= 1;
            }
            persist_alarms();
            alarmd_reload();
            return;
        }
        // 点击行 = 编辑。
        unsafe {
            let a = ALARMS[i];
            EDIT_ID = a.id;
            EDIT_NEW = false;
            EDIT_HH = a.hh;
            EDIT_MM = a.mm;
            EDIT_REP = a.repeat;
            EDIT_LABEL = a.label;
            EDIT_LABEL_LEN = a.label_len;
            EDIT_FOCUS = 0;
            APAGE = APAGE_EDIT;
        }
        return;
    }
}

fn tap_edit(x: i32, y: i32) {
    // 小时加减。
    if x >= 60 && x < 120 && y >= 200 && y < 240 {
        unsafe { EDIT_HH = (EDIT_HH + 1) % 24 };
        return;
    }
    if x >= 60 && x < 120 && y >= 286 && y < 326 {
        unsafe { EDIT_HH = (EDIT_HH + 23) % 24 };
        return;
    }
    // 分钟加减。
    if x >= 350 && x < 410 && y >= 200 && y < 240 {
        unsafe { EDIT_MM = (EDIT_MM + 1) % 60 };
        return;
    }
    if x >= 350 && x < 410 && y >= 286 && y < 326 {
        unsafe { EDIT_MM = (EDIT_MM + 59) % 60 };
        return;
    }
    // 周重复。
    let chip_w = (DISPLAY_W as i32 - 52) / 7;
    let cy = 316;
    if y >= cy && y < cy + 38 {
        for i in 0..7 {
            let x0 = 26 + i as i32 * chip_w;
            if x >= x0 && x < x0 + chip_w - 4 {
                unsafe {
                    EDIT_REP ^= 1 << i;
                }
                break;
            }
        }
        return;
    }
    // 快速选项。
    let cw = (DISPLAY_W as i32 - 52 - 12) / 3;
    let qy = 370;
    if y >= qy && y < qy + 36 {
        for i in 0..3 {
            let x0 = 26 + i as i32 * (cw + 6);
            if x >= x0 && x < x0 + cw - 6 {
                unsafe {
                    EDIT_REP = match i {
                        0 => 0x7F,
                        1 => 0x1F,
                        _ => 0,
                    };
                }
                break;
            }
        }
        return;
    }
    // 标签输入框焦点。
    let ly = 418;
    if y >= ly + 12 && y < ly + 54 {
        unsafe { EDIT_FOCUS = if EDIT_FOCUS == 1 { 0 } else { 1 } };
        return;
    }
    // 取消 / 保存。
    let by = content_bottom() - 54;
    if y >= by && y < by + 44 {
        if x >= 26 && x < 206 {
            unsafe { APAGE = APAGE_LIST };
            return;
        }
        if x >= DISPLAY_W as i32 - 206 && x < DISPLAY_W as i32 - 26 {
            save_alarm();
            return;
        }
    }
}

fn save_alarm() {
    unsafe {
        if EDIT_NEW {
            let mut a = Alarm::empty();
            a.id = EDIT_ID;
            a.on = true;
            a.hh = EDIT_HH;
            a.mm = EDIT_MM;
            a.repeat = EDIT_REP;
            a.label = EDIT_LABEL;
            a.label_len = EDIT_LABEL_LEN;
            if ALARM_COUNT < MAX_ALARMS {
                ALARMS[ALARM_COUNT] = a;
                ALARM_COUNT += 1;
            }
        } else {
            for i in 0..ALARM_COUNT {
                if ALARMS[i].id == EDIT_ID {
                    let a = &mut ALARMS[i];
                    a.hh = EDIT_HH;
                    a.mm = EDIT_MM;
                    a.repeat = EDIT_REP;
                    a.label = EDIT_LABEL;
                    a.label_len = EDIT_LABEL_LEN;
                    break;
                }
            }
        }
        APAGE = APAGE_LIST;
    }
    persist_alarms();
    alarmd_reload();
}

fn tap_timer(x: i32, y: i32) {
    let by = content_bottom() - 60;
    // 预设。
    let pw = (DISPLAY_W as i32 - 52 - 30) / 6;
    for i in 0..6 {
        let x0 = 26 + i as i32 * (pw + 6);
        if x >= x0 && x < x0 + pw - 6 && y >= 336 && y < 376 {
            unsafe {
                let val = [1u32, 3, 5, 10, 15, 30][i];
                TMR_MIN = val;
                if TMR_STATE == 0 {
                    TMR_TOTAL = val as u64 * 60;
                    TMR_REMAIN = TMR_TOTAL;
                }
            }
            return;
        }
    }
    if y < by || y >= by + 46 {
        return;
    }
    // 左按钮。
    if x >= 30 && x < 180 {
        unsafe {
            match TMR_STATE {
                0 => {
                    TMR_TOTAL = TMR_MIN as u64 * 60;
                    TMR_START_EP = now_us() / 1_000_000;
                    TMR_STATE = 1;
                }
                1 => {
                    TMR_REMAIN = timer_remaining();
                    TMR_STATE = 2;
                }
                2 => {
                    TMR_START_EP = now_us() / 1_000_000;
                    TMR_TOTAL = TMR_REMAIN;
                    TMR_STATE = 1;
                }
                _ => {
                    TMR_STATE = 0;
                    TMR_TOTAL = TMR_MIN as u64 * 60;
                    TMR_REMAIN = TMR_TOTAL;
                }
            }
        }
        return;
    }
    // 右按钮。
    if x >= 360 && x < 510 {
        unsafe {
            if TMR_STATE == 3 {
                // 再来：按当前分钟重启。
                TMR_STATE = 1;
                TMR_TOTAL = TMR_MIN as u64 * 60;
                TMR_START_EP = now_us() / 1_000_000;
            } else {
                TMR_STATE = 0;
                TMR_TOTAL = TMR_MIN as u64 * 60;
                TMR_REMAIN = TMR_TOTAL;
            }
        }
        return;
    }
}

fn timer_tick() {
    unsafe {
        if TMR_STATE == 1 && timer_remaining() == 0 {
            TMR_STATE = 3;
            TMR_REMAIN = 0;
            beep_finish();
        }
    }
}

fn tap_stopwatch(x: i32, y: i32) {
    let by = content_bottom() - 60;
    if y < by || y >= by + 46 {
        return;
    }
    if x >= 30 && x < 180 {
        unsafe {
            if SW_RUN {
                let ep = now_us();
                SW_ACC_US += ep.saturating_sub(SW_RESUME_EP);
                SW_RUN = false;
            } else {
                SW_RESUME_EP = now_us();
                SW_RUN = true;
            }
        }
        return;
    }
    if x >= 360 && x < 510 {
        unsafe {
            if SW_RUN {
                // 计次。
                if SW_LAPS < 10 {
                    let total = sw_elapsed_ms() as u32;
                    let prev = if SW_LAPS == 0 {
                        0
                    } else {
                        SW_LAP_TOTAL[SW_LAPS - 1]
                    };
                    let lap = total.saturating_sub(prev);
                    SW_LAP_MS[SW_LAPS] = lap;
                    SW_LAP_TOTAL[SW_LAPS] = total;
                    SW_LAPS += 1;
                }
            } else if SW_LAPS > 0 {
                SW_ACC_US = 0;
                SW_LAPS = 0;
                SW_RUN = false;
            }
        }
    }
}

fn tap_ringing(x: i32, y: i32) {
    let cx = DISPLAY_W as i32 / 2;
    let cy = (92 + content_bottom()) / 2;
    let id = unsafe { ARING_ID };
    if y >= cy + 70 && y < cy + 118 {
        if x >= cx - 200 && x < cx {
            alarmd_cmd_id(b"SNOOZE", id);
            return;
        }
        if x >= cx && x < cx + 200 {
            alarmd_cmd_id(b"DISMISS", id);
            return;
        }
    }
}

// ==================== 键盘输入 ====================

fn handle_key(key: KeyAction) {
    unsafe {
        let editing = TAB == TAB_ALARM && APAGE == APAGE_EDIT;
        if editing && EDIT_FOCUS == 1 {
            match key {
                KeyAction::Char(c) => {
                    if EDIT_LABEL_LEN < 30 {
                        EDIT_LABEL[EDIT_LABEL_LEN] = c;
                        EDIT_LABEL_LEN += 1;
                        EDIT_DIRTY = true;
                    }
                }
                KeyAction::Backspace => {
                    if EDIT_LABEL_LEN > 0 {
                        EDIT_LABEL_LEN -= 1;
                        EDIT_DIRTY = true;
                    }
                }
                KeyAction::Enter => {
                    EDIT_FOCUS = 0;
                }
                _ => {}
            }
            return;
        }
        match key {
            KeyAction::Enter => {
                if TAB == TAB_ALARM && APAGE == APAGE_EDIT {
                    save_alarm();
                }
            }
            _ => {}
        }
    }
}

// ==================== 主函数 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[clock] 启动中\n");
    if !fb_init() {
        print("[clock] /dev/fb0 内存映射失败，无帧缓冲\n");
        unsafe { exit(1); }
    }
    load_alarms();
    let _ = current_zone();
    print("[clock] 就绪\n");

    let mut last_sec = u32::MAX;
    let mut dirty = true;

    loop {
        let ev = poll_input();
        if ev.moved {
            cursor_overlay_move();
        }
        if ev.nav != NavAction::None {
            print("[nav] 系统导航 → 退出\n");
            unsafe { exit(nav_exit_code(ev.nav)); }
        }
        if ev.key != KeyAction::None {
            handle_key(ev.key);
        }
        if let Some((x, y)) = ev.tap {
            if back_hit(x, y) {
                print("[clock] 返回 → 退出\n");
                break;
            }
            let ringing = unsafe { ASTATE } == 1;
            if ringing {
                tap_ringing(x, y);
            } else if y >= TAB_Y && y < TAB_Y + TAB_H {
                unsafe {
                    let nt = (x / (DISPLAY_W as i32 / 4)).min(3) as u8;
                    if nt != TAB {
                        // 标签切换：指示药丸从旧位滑向新位
                        TAB_PREV = TAB;
                        TAB_ANIM_START = now_us();
                        TAB = nt;
                    }
                    if TAB != TAB_ALARM {
                        APAGE = APAGE_LIST;
                    }
                    SCROLL = 0;
                }
            } else {
                match unsafe { TAB } {
                    TAB_WORLD => tap_world(x, y),
                    TAB_ALARM => {
                        if unsafe { APAGE } == APAGE_EDIT {
                            tap_edit(x, y);
                        } else {
                            tap_alarm_list(x, y);
                        }
                    }
                    TAB_TIMER => tap_timer(x, y),
                    _ => tap_stopwatch(x, y),
                }
            }
        }
        if ev.key == KeyAction::Esc || poll_tty_esc() {
            print("[clock] ESC → 退出\n");
            break;
        }
        let (_, _, s) = now_hms();
        if s != last_sec {
            last_sec = s;
            dirty = true;
        }
        if unsafe { EDIT_DIRTY } {
            unsafe { EDIT_DIRTY = false };
            dirty = true;
        }
        // 计时到点检测。
        timer_tick();
        read_alarmd_state();
        // 世界时钟 / 秒表 / 计时器需要持续刷新；每秒或交互时刷新。
        let need_anim = unsafe {
            TAB == TAB_STOPWATCH
                || (TAB == TAB_TIMER && TMR_STATE == 1)
                || TAB == TAB_WORLD
        };
        if need_anim {
            dirty = true;
        }
        // 标签切换动画期内逐帧重绘（主循环节奏 32ms）
        if tab_anim_active() {
            dirty = true;
        }
        if dirty {
            render();
            flip();
            cursor_overlay_redraw();
            dirty = false;
            kit::font_init_checked();
            if kit::font_just_refreshed() { dirty = true; }
        }
        sleep_ms(32);
    }

    print("[clock] 正在退出\n");
    unsafe { exit(0); }
}

// ==================== 计时完成蜂鸣（audiod） ====================

fn beep_finish() {
    if audio::open_stream(audio::USAGE_MUSIC) != 0 {
        return;
    }
    audio::request_focus();
    let mut out = [0u8; audio::CHUNK_BYTES];
    // 1568Hz 三声 200ms，间隔 100ms。
    for c in 0..60 {
        synth_beep(c, &mut out);
        let r = audio::write_chunk(&out);
        if r != 0 {
            break;
        }
        sleep_ms(10);
    }
    audio::release_focus();
    audio::close_stream();
}

fn synth_beep(chunk: u32, out: &mut [u8]) {
    out.fill(0);
    let freq = 1568u32;
    let step = (freq as u64 * 256 / audio::SAMPLE_RATE as u64) as u32;
    if step == 0 {
        return;
    }
    let base = chunk as u64 * audio::CHUNK_FRAMES as u64 * step as u64;
    for i in 0..audio::CHUNK_FRAMES {
        let phase = (base + i as u64 * step as u64) & 255;
        // 多项式正弦（无 libm）。
        let mut x = phase as f32 / 256.0 * 2.0 * core::f32::consts::PI;
        let neg = x > core::f32::consts::PI;
        if neg {
            x -= core::f32::consts::PI;
        }
        if x > core::f32::consts::PI / 2.0 {
            x = core::f32::consts::PI - x;
        }
        let num = 16.0 * x * (core::f32::consts::PI - x);
        let den = 5.0 * core::f32::consts::PI * core::f32::consts::PI - 4.0 * x * (core::f32::consts::PI - x);
        let mut s = if den != 0.0 { num / den } else { 0.0 };
        if neg {
            s = -s;
        }
        // 节奏：响 20 块，停 10 块。
        let in_beep = (chunk % 30) < 20;
        let mut v = (s * 24000.0) as i32;
        if !in_beep {
            v = 0;
        }
        let o = i * 4;
        let s16 = v.clamp(-32767, 32767) as i16;
        let u = s16 as u16;
        out[o] = (u & 0xFF) as u8;
        out[o + 1] = ((u >> 8) & 0xFF) as u8;
        out[o + 2] = (u & 0xFF) as u8;
        out[o + 3] = ((u >> 8) & 0xFF) as u8;
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
        write_fd(2, b"PANIC: clock\n");
        write_fd(2, &w.buf[..w.len]);
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}