// KunxOS 桌面应用图标贴图（no_std，无堆）。
//
// 21 个应用的 56x56 RGBA8888 图标（行主序，应用间紧邻），顺序与
// apps/launcher/src/main.rs 的 APPS 表严格一致。由 tools/make_icons.py
// 绘制生成 shared/app_icons.rgba，随 rootfs 以 /share/app_icons.rgba
// 提供，launcher 启动时读入并 install()。
//
// 为什么不内嵌 ELF（与 cursor.rgba 不同）：贴图 263KB 会把 launcher
// ELF 顶过内核 execve 的读取缓冲上限 EXEC_BUF_CAP（kunxcore/src/elf.rs，
// 256KB）导致应用无法启动；与系统字体一样走"rootfs 文件 + 运行时读取"，
// 换图标重跑 make_icons.py 即可，无上限约束。
//
// 像素约定：alpha=0 为全透明（圆角外），255 为不透明；launcher 填充图标
// 瓦片缓存时把半透明边缘的 alpha 编码进颜色最高位字节，blit 时与背景混合。

/// 贴图边长（必须与 tools/make_icons.py 的 ICON 一致）。
pub const ICON_PX: usize = 56;
/// 应用数（必须与 apps/launcher 的 APPS 表长度一致）。
pub const APP_ICON_COUNT: usize = 21;
/// 单应用贴图字节数（RGBA8888）。
pub const ICON_BYTES: usize = ICON_PX * ICON_PX * 4;
/// 全部贴图总字节数（= /share/app_icons.rgba 文件大小）。
pub const ALL_BYTES: usize = APP_ICON_COUNT * ICON_BYTES;

static mut ICONS: Option<&'static [u8]> = None;

/// 安装整份贴图（零拷贝，借用调用方的 'static 缓冲；长度必须恰好等于
/// ALL_BYTES）。
pub fn install(data: &'static [u8]) -> bool {
    if data.len() != ALL_BYTES {
        return false;
    }
    unsafe {
        ICONS = Some(data);
    }
    true
}

/// 第 idx 个应用的贴图（RGBA8888 裸像素，行主序；未安装或越界返回 None）。
#[inline(always)]
pub fn icon_rgba(idx: usize) -> Option<&'static [u8]> {
    let d = unsafe { ICONS }?;
    if idx >= APP_ICON_COUNT {
        return None;
    }
    let n = ICON_BYTES;
    Some(&d[idx * n..(idx + 1) * n])
}
