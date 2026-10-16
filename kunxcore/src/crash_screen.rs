//! 崩溃告知屏：图形界面死亡（launcher 崩溃循环放弃 / init 监督耗尽）后，
//! 由内核直接在帧缓冲上向用户告知——「系统已降级但仍在运行」+ 修复指引。
//!
//! 设计约束：
//!   - 只在 GUI 确认死亡后调用一次（kmain 回退控制台 shell 时）；内核此后
//!     不再刷新屏幕（静态画面），控制台继续可用；
//!   - 字体经 ui_framework::font_init 从 rootfs 加载（失败时退化为纯色屏
//!     + 控制台文本，告知不丢——串口仍有完整信息）；
//!   - 绘制走 compositor Surface（与 GUI 同一帧缓冲），不动用户态状态。

use crate::compositor::{Color, Surface};
use crate::gpu_simple;

/// 显示崩溃告知屏。`reason` 为一行原因，`extra` 为附加说明行。
pub fn show(reason: &str, extra: &[&str]) {
    let base = gpu_simple::fb_base();
    let w = gpu_simple::width();
    let h = gpu_simple::height();
    // 控制台副本（无 fb / 字体失败时这是唯一告知渠道）
    crate::uart_send_str("[crash-screen] ");
    crate::uart_send_str(reason);
    crate::uart_send_str("\n");
    // 健康事件先于 fb 绘制上报（无帧缓冲的提前返回不得吞掉告知）
    crate::health::report(
        crate::health::SEV_CRITICAL,
        "display",
        "图形界面已停止，系统降级为控制台模式",
        "重启设备通常可恢复；反复出现请查看 /data/crash 或重新刷机",
    );
    if base == 0 || w == 0 || h == 0 {
        return; // 无帧缓冲（无头环境）：控制台已告知
    }
    // 字体按需加载（内核常态不用 UI；GUI 活着时它有自己的字体实例）
    crate::ui_framework::font_init();

    let s = Surface {
        pixels: base as *mut u32,
        w,
        h,
    };
    // 深色底 + 顶部警示条
    s.fill(Color::rgb(16, 16, 20));
    let bar = Color::rgb(180, 60, 30);
    s.fill_rect(crate::compositor::Rect::new(0, 0, w, 8), bar);

    let mut y: i32 = 96;
    let cx = |text: &str| (w as i32 - crate::ui_framework::text_width_bold(text)) / 2;
    // 标题
    let title = "系统遇到严重问题";
    crate::ui_framework::draw_text_bold(&s, cx(title).max(24), y, title, Color::WHITE, 2);
    y += 72;
    // 原因（琥珀）
    let rc = Color::rgb(255, 179, 0);
    crate::ui_framework::draw_text(&s, 40, y, reason, rc, 1);
    y += 40;
    // 附加说明（浅灰）
    let body = Color::rgb(210, 210, 210);
    for line in extra {
        crate::ui_framework::draw_text(&s, 40, y, line, body, 1);
        y += 32;
    }
    // 固定指引
    y += 24;
    let guide: [&str; 4] = [
        "系统已自动降级为控制台模式，仍在运行，",
        "未接串口的用户请长按电源键重启设备。",
        "重启通常可恢复图形界面；若反复出现本屏，",
        "请查看 /data/crash 崩溃报告或重新刷机。",
    ];
    for line in guide {
        crate::ui_framework::draw_text(&s, 40, y, line, Color::GRAY, 1);
        y += 30;
    }
    // 健康事件计数提示
    let crit = crate::health::critical_count();
    if crit > 0 {
        let mut b = [0u8; 48];
        let msg = make_count_line(crit, &mut b);
        crate::ui_framework::draw_text(&s, 40, y + 12, msg, Color::GRAY, 1);
    }
    // 翻转为扫描输出（若尚未显示）
    gpu_simple::present(base);
}

fn make_count_line<'a>(crit: u32, buf: &'a mut [u8; 48]) -> &'a str {
    let mut n = 0usize;
    for &c in "本次开机累计严重故障 ".as_bytes() {
        if n < buf.len() {
            buf[n] = c;
            n += 1;
        }
    }
    let mut pb = [0u8; 10];
    let mut i = pb.len();
    let mut v = crit as u64;
    loop {
        i -= 1;
        pb[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    for &c in &pb[i..] {
        if n < buf.len() {
            buf[n] = c;
            n += 1;
        }
    }
    for &c in " 次（串口 `health` 可查详情）".as_bytes() {
        if n < buf.len() {
            buf[n] = c;
            n += 1;
        }
    }
    core::str::from_utf8(&buf[..n]).unwrap_or("")
}
