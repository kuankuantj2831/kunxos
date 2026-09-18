//! Phosphor 图标字体码点表（source of truth，勿手改码点）。
//!
//! 两个源字体（fonts/）：`Phosphor-Fill.ttf` 实心风格、`Phosphor-Bold.ttf`
//! 粗线风格。Phosphor 对同一图标在全部字重使用相同码点（PUA 区
//! U+E000..=U+F8FF），因此一份码点表通用于两种风格。码点取自
//! @phosphor-icons/core 2.1.1 官方 meta（与字体 Version 2.1 一致）。
//!
//! 用途：
//! - 软件图标：tools/make_icons.py 读取 `app` 模块常量，把 Phosphor-Fill
//!   字形栅格化进桌面图标贴图（shared/app_icons.rgba）；
//! - 行内 emoji：font_render 在渲染文本时把 emoji 码点经 [`emoji_icon`]
//!   映射为 Phosphor-Bold 图标（单色），缺字形/未映射的码点行为不变。
//!
//! 新增图标：加常量/映射后重跑 `python3 tools/make_fonts.py`（icon 子集）
//! 与 `python3 tools/make_icons.py`（桌面贴图）。子集只含本文件出现的
//! 码点 —— 忘记重生成时对应图标会静默缺失（has_glyph=false，不画）。

// ==================== 应用桌面图标（Phosphor-Fill） ====================

/// 应用桌面图标码点（实心风格；与启动器 APPS 表 key 对应，
/// 由 tools/make_icons.py 按 key 读取渲染进贴图）。
pub mod app {
    /// 电话（phone-call）。
    pub const PHONE: u32 = 0xE3BA;
    /// 短信（chat-circle-dots）。
    pub const SMS: u32 = 0xE16C;
    /// 通讯录（address-book）。
    pub const CONTACTS: u32 = 0xE6F8;
    /// 设置（gear-six）。
    pub const SETTINGS: u32 = 0xE272;
    /// 文件（folder-simple）。
    pub const FILES: u32 = 0xE25A;
    /// 浏览器（globe-hemisphere-west）。
    pub const BROWSER: u32 = 0xE28C;
    /// 计算器（calculator）。
    pub const CALC: u32 = 0xE538;
    /// 日历（calendar）。
    pub const CALENDAR: u32 = 0xE108;
    /// 相机（camera）。
    pub const CAMERA: u32 = 0xE10E;
    /// 时钟（clock）。
    pub const CLOCK: u32 = 0xE19A;
    /// 图库（image）。
    pub const GALLERY: u32 = 0xE2CA;
    /// 音乐（music-notes）。
    pub const MUSIC: u32 = 0xE340;
    /// 邮件（envelope-simple）。
    pub const MAIL: u32 = 0xE218;
    /// 地图（map-trifold）。
    pub const MAP: u32 = 0xE31A;
    /// 天气（cloud-sun）。
    pub const WEATHER: u32 = 0xE540;
    /// 应用商店（storefront）。
    pub const STORE: u32 = 0xE470;
    /// 终端（terminal-window）。
    pub const TERMINAL: u32 = 0xEAE8;
    /// 云服务（cloud-arrow-up）。
    pub const CLOUD: u32 = 0xE1AE;
    /// 视频（video-camera）。
    pub const VIDEO: u32 = 0xE4DA;
    /// 备忘录（note-pencil）。
    pub const NOTES: u32 = 0xE34C;
    /// LCApp（app-window）。
    pub const LCAPP: u32 = 0xE5DA;
}

/// 桌面图标 key → 码点（tools/make_icons.py 同步使用；key 与启动器 APPS 表一致）。
pub fn app_icon(key: &[u8]) -> Option<u32> {
    let icon = match key {
        b"phone" => app::PHONE,
        b"sms" => app::SMS,
        b"contacts" => app::CONTACTS,
        b"settings" => app::SETTINGS,
        b"files" => app::FILES,
        b"browser" => app::BROWSER,
        b"calc" => app::CALC,
        b"calendar" => app::CALENDAR,
        b"camera" => app::CAMERA,
        b"clock" => app::CLOCK,
        b"gallery" => app::GALLERY,
        b"music" => app::MUSIC,
        b"mail" => app::MAIL,
        b"map" => app::MAP,
        b"weather" => app::WEATHER,
        b"store" => app::STORE,
        b"terminal" => app::TERMINAL,
        b"cloud" => app::CLOUD,
        b"video" => app::VIDEO,
        b"notes" => app::NOTES,
        b"lcapp" => app::LCAPP,
        _ => return None,
    };
    Some(icon)
}

// ==================== 天气图标（Phosphor-Fill） ====================

/// 天气图标码点（实心风格；桌面天气小组件/天气应用使用）。
pub mod weather {
    /// 晴（sun）。
    pub const SUN: u32 = 0xE472;
    /// 多云/阴（cloud）。
    pub const CLOUD: u32 = 0xE1AA;
    /// 雨（cloud-rain）。
    pub const RAIN: u32 = 0xE1B4;
    /// 雪（cloud-snow）。
    pub const SNOW: u32 = 0xE1B8;
    /// 雷雨（cloud-lightning）。
    pub const STORM: u32 = 0xE1B2;
    /// 夜（moon-stars）。
    pub const MOON: u32 = 0xE58E;
    /// 彩虹（rainbow）。
    pub const RAINBOW: u32 = 0xE598;
    /// 风（wind）。
    pub const WIND: u32 = 0xE5D2;
    /// 湿度（drop）。
    pub const DROP: u32 = 0xE210;
}

// ==================== 通用 UI 图标（Phosphor-Bold 粗线） ====================

/// 通用 UI 小图标码点（粗线风格，小尺寸比实心更清晰）。
pub mod ui {
    pub const CHECK: u32 = 0xE182; // check
    pub const CHECK_CIRCLE: u32 = 0xE184; // check-circle
    pub const X: u32 = 0xE4F6; // x
    pub const X_CIRCLE: u32 = 0xE4F8; // x-circle
    pub const WARNING: u32 = 0xE4E0; // warning
    pub const INFO: u32 = 0xE2CE; // info
    pub const QUESTION: u32 = 0xE3E8; // question
    pub const ARROW_LEFT: u32 = 0xE058; // arrow-left
    pub const ARROW_RIGHT: u32 = 0xE06C; // arrow-right
    pub const ARROW_UP: u32 = 0xE08E; // arrow-up
    pub const ARROW_DOWN: u32 = 0xE03E; // arrow-down
    pub const GEAR: u32 = 0xE270; // gear
    pub const SEARCH: u32 = 0xE30C; // magnifying-glass
    pub const BELL: u32 = 0xE0CE; // bell
    pub const BELL_OFF: u32 = 0xE0D4; // bell-slash
    pub const LOCK: u32 = 0xE2FA; // lock
    pub const LOCK_OPEN: u32 = 0xE306; // lock-open
    pub const KEY: u32 = 0xE2D6; // key
    pub const USER: u32 = 0xE4C2; // user
    pub const USERS: u32 = 0xE4D6; // users
    pub const ENVELOPE: u32 = 0xE218; // envelope-simple
    pub const CHAT: u32 = 0xE16C; // chat-circle-dots
    pub const PHONE: u32 = 0xE3B8; // phone
    pub const FOLDER: u32 = 0xE25A; // folder-simple
    pub const FILE: u32 = 0xE23A; // file-text
    pub const CALENDAR: u32 = 0xE108; // calendar
    pub const CLOCK: u32 = 0xE19A; // clock
    pub const HEART: u32 = 0xE2A8; // heart
    pub const STAR: u32 = 0xE46A; // star
    pub const TRASH: u32 = 0xE4A6; // trash
    pub const PLUS: u32 = 0xE3D4; // plus
    pub const MINUS: u32 = 0xE32A; // minus
    pub const PLAY: u32 = 0xE3D0; // play
    pub const PAUSE: u32 = 0xE39E; // pause
    pub const WIFI: u32 = 0xE4EA; // wifi-high
    pub const BATTERY: u32 = 0xE0C0; // battery-full
    pub const BLUETOOTH: u32 = 0xE0DA; // bluetooth
    pub const SUN: u32 = 0xE472; // sun
    pub const MOON: u32 = 0xE330; // moon
    pub const HOME: u32 = 0xE2C2; // house
    pub const MAP_PIN: u32 = 0xE316; // map-pin
    pub const GLOBE: u32 = 0xE288; // globe
    pub const LIGHTNING: u32 = 0xE2DE; // lightning
    pub const COPY: u32 = 0xE1CA; // copy
    pub const SHARE: u32 = 0xE408; // share-network
}

// ==================== 设置应用图标（Phosphor-Bold） ====================

/// 设置应用行图标码点（粗线风格，24px 圆角底上按 16px 栅格化；与
/// apps/settings/src/ui.rs 的 ICONS 表按索引对应）。返回箭头（caret-left）、
/// 列表箭头（caret-right）、搜索清除（x）也在此集中定义。
pub mod settings {
    /// WLAN（wifi-high）。
    pub const WIFI: u32 = 0xE4EA;
    /// 蜂窝网络（cell-signal-full）。
    pub const MOBILE: u32 = 0xE142;
    /// VPN（shield-check）。
    pub const VPN: u32 = 0xE40C;
    /// 蓝牙（bluetooth）。
    pub const BT: u32 = 0xE0DA;
    /// 显示/亮度（sun）。
    pub const SUN: u32 = 0xE472;
    /// 深色模式（moon）。
    pub const MOON: u32 = 0xE330;
    /// 字体大小（text-aa）。
    pub const FONT: u32 = 0xE6EE;
    /// 声音（speaker-high）。
    pub const SPEAK: u32 = 0xE44A;
    /// 通知（bell）。
    pub const BELL: u32 = 0xE0CE;
    /// 电池（battery-full）。
    pub const BATT: u32 = 0xE0C0;
    /// 存储（hard-drive）。
    pub const DISK: u32 = 0xE29E;
    /// 隐私/指纹（fingerprint）。
    pub const FINGER: u32 = 0xE23E;
    /// 锁屏（lock）。
    pub const LOCK: u32 = 0xE2FA;
    /// 系统更新（download-simple）。
    pub const UPDATE: u32 = 0xE20C;
    /// 电话（phone）。
    pub const PHONE: u32 = 0xE3B8;
    /// 系统导航（arrows-left-right）。
    pub const NAV: u32 = 0xE0A0;
    /// 应用管理（squares-four）。
    pub const APPS: u32 = 0xE464;
    /// 恢复出厂（arrow-counter-clockwise）。
    pub const RESET: u32 = 0xE038;
    /// 账户（user）。
    pub const USER: u32 = 0xE4C2;
    /// 云服务（cloud）。
    pub const CLOUD: u32 = 0xE1AA;
    /// 同步（arrows-clockwise）。
    pub const SYNC: u32 = 0xE094;
    /// 时区（clock）。
    pub const CLOCK: u32 = 0xE19A;
    /// 语言（translate）。
    pub const LANG: u32 = 0xE4A2;
    /// 键盘（keyboard）。
    pub const KBD: u32 = 0xE2D8;
    /// 图案解锁（dots-nine）。
    pub const PAT: u32 = 0xE1FC;
    /// 关于（info）。
    pub const INFO: u32 = 0xE2CE;
    /// 无障碍（person-arms-spread）。
    pub const ACC: u32 = 0xECFE;
    /// 显存/眼睛（eye）。
    pub const EYE: u32 = 0xE220;
    /// 开发者（gear）。
    pub const GEAR: u32 = 0xE270;
    /// 保存（floppy-disk）。
    pub const SAVE: u32 = 0xE248;
    /// 输入法（cursor-text）。
    pub const INPUT: u32 = 0xE7D8;
    /// 返回箭头（caret-left，标题栏）。
    pub const CARET_LEFT: u32 = 0xE138;
    /// 列表右箭头（caret-right）。
    pub const CARET_RIGHT: u32 = 0xE13A;
    /// 清除（x，搜索框）。
    pub const X: u32 = 0xE4F6;
}

// ==================== Emoji → Phosphor 映射 ====================

/// 行内 emoji 占位（变体选择符 VS16 / 零宽连接符 ZWJ / 肤色修饰 / 区域
/// 指示符）：参与 emoji 序列但不独立呈现，渲染为零宽空位。
pub fn is_hidden_cp(cp: u32) -> bool {
    matches!(cp,
        0x200D | 0xFE0F
        | 0x1F3FB..=0x1F3FF  // 肤色修饰
        | 0x1F1E6..=0x1F1FF) // 区域指示符（国旗，单色字体无法呈现）
}

/// Emoji 码点 → Phosphor 图标码点（Phosphor-Bold 渲染）。
/// None = 未映射：退回常规字体（与旧行为一致，cn.ttf 有则照常显示）。
pub fn emoji_icon(cp: u32) -> Option<u32> {
    let icon = match cp {
        // 笑脸（Phosphor 表情有限，按情绪就近映射）
        0x1F600 | 0x1F601 | 0x1F602 | 0x1F603 | 0x1F604 | 0x1F605 | 0x1F606 | 0x1F607 | 0x1F60A | 0x1F60B | 0x1F60D | 0x1F62E => 0xE436, // smiley
        0x1F609 | 0x1F61C | 0x1F61D => 0xE666, // smiley-wink
        0x1F610 | 0x1F611 | 0x1F636 | 0x1F62F => 0xE438, // smiley-blank
        0x1F60C | 0x1F612 | 0x1F613 | 0x1F62A | 0x1F62B | 0x1F62C | 0x1F634 => 0xE43A, // smiley-meh
        0x1F614 | 0x1F615 | 0x1F616 | 0x1F61E | 0x1F61F | 0x1F622 | 0x1F623 | 0x1F641 => 0xE43E, // smiley-sad
        0x1F630 | 0x1F633 => 0xE43C, // smiley-nervous
        0x1F631 | 0x1F635 => 0xE442, // smiley-x-eyes
        0x1F620 | 0x1F621 => 0xEC62, // smiley-angry
        0x1F975 => 0xEE56, // smiley-melting
        0x1F60E => 0xE816, // sunglasses
        0x1F47B => 0xE62A, // ghost
        0x1F916 => 0xE762, // robot
        0x1F47D | 0x1F47E => 0xE8A6, // alien
        0x1F480 | 0x2620 => 0xE916, // skull
        // 各类彩色心形统一映射
        0x2763 | 0x2764 | 0x2665 | 0x1F495 | 0x1F496 | 0x1F497 | 0x1F498 | 0x1F49A | 0x1F49B | 0x1F49C | 0x1F49D | 0x1F49E | 0x1F49F => 0xE2A8, // heart
        0x1F493 => 0xE2AC, // heartbeat
        0x1F494 => 0xEBE8, // heart-break
        0x2B50 => 0xE46A, // star
        0x2728 | 0x1F31F | 0x2747 | 0x2734 => 0xE6A2, // sparkle
        0x2733 => 0xE0AA, // asterisk
        0x1F4AB => 0xECFA, // shooting-star
        0x2600 | 0x1F31E | 0x1F323 => 0xE472, // sun
        0x2601 | 0x2602 | 0x1F325 => 0xE1AA, // cloud
        0x26C5 | 0x1F324 => 0xE540, // cloud-sun
        0x1F326 | 0x1F327 => 0xE1B4, // cloud-rain
        0x1F328 => 0xE1B8, // cloud-snow
        0x2744 | 0x2745 | 0x26C4 => 0xE5AA, // snowflake
        0x26C8 | 0x1F329 => 0xE1B2, // cloud-lightning
        0x26A1 => 0xE2DE, // lightning
        0x1F32A => 0xE88C, // tornado
        0x1F32B => 0xE53C, // cloud-fog
        0x1F32C | 0x1F4A8 => 0xE5D2, // wind
        0x2614 => 0xE684, // umbrella
        0x1F308 => 0xE598, // rainbow
        0x1F319 | 0x1F31A => 0xE330, // moon
        0x1F31B | 0x1F31C => 0xE58E, // moon-stars
        0x1F30D | 0x1F30E | 0x1F30F | 0x1F310 => 0xE288, // globe
        0x1F4A7 | 0x1F4A6 => 0xE210, // drop
        0x1F525 => 0xE242, // fire
        0x1F33F | 0x1F340 | 0x1F341 | 0x1F342 | 0x1F343 => 0xE2DA, // leaf
        0x1F332 | 0x1F333 => 0xE6DA, // tree
        0x1F334 => 0xE91A, // tree-palm
        0x1F335 => 0xE918, // cactus
        0x1F339 | 0x1F33A | 0x1F33B | 0x1F490 => 0xE75E, // flower
        0x1F337 => 0xEACC, // flower-tulip
        0x231A => 0xE4E6, // watch
        0x23F0 => 0xE006, // alarm
        0x23F1 => 0xE492, // timer
        0x231B | 0x23F3 => 0xE2B2, // hourglass
        0x1F4F1 => 0xE1E0, // device-mobile
        0x1F4BB => 0xE586, // laptop
        0x1F5A5 => 0xE32E, // monitor
        0x2328 => 0xE2D8, // keyboard
        0x1F5A8 => 0xE3DC, // printer
        0x1F4FA => 0xE754, // television
        0x1F4FB => 0xE77E, // radio
        0x1F3A7 => 0xE2A6, // headphones
        0x1F3A4 => 0xE326, // microphone
        0x1F4F7 | 0x1F4F8 => 0xE10E, // camera
        0x1F3AE => 0xE26E, // game-controller
        0x1F579 => 0xEA5E, // joystick
        0x1F3B2 => 0xE1EE, // dice-five
        0x1F3AF => 0xE47C, // target
        0x1F3C6 => 0xE67E, // trophy
        0x1F451 => 0xE614, // crown
        0x1F453 => 0xE7BA, // eyeglasses
        0x1F4A1 => 0xE2DC, // lightbulb
        0x1F50B => 0xE0C0, // battery-full
        0x1F50C => 0xE946, // plug
        0x1F4E1 => 0xE0F2, // broadcast
        0x1F50D | 0x1F50E => 0xE30C, // magnifying-glass
        0x1F527 => 0xE5D4, // wrench
        0x1F528 => 0xE80E, // hammer
        0x1F52C => 0xEC7A, // microscope
        0x1F9EA => 0xE79E, // flask
        0x1F4A3 => 0xEE0A, // bomb
        0x1F511 => 0xE2D6, // key
        0x1F512 | 0x1F510 => 0xE2FA, // lock
        0x1F513 => 0xE306, // lock-open
        0x1F6AA => 0xE7E6, // door-open
        0x1F4B3 => 0xE1D2, // credit-card
        0x1F4B0 => 0xE588, // money
        0x1F4BC => 0xE0EE, // briefcase
        0x1F381 => 0xE276, // gift
        0x1F388 => 0xE76C, // balloon
        0x1F389 | 0x1F38A => 0xE81A, // confetti
        0x1F3C5 => 0xE320, // medal
        0x1F516 => 0xE0EA, // bookmark-simple
        0x1F4BF | 0x1F4C0 => 0xECAC, // vinyl-record
        0x260E => 0xE3B8, // phone
        0x1F4DE => 0xE3BA, // phone-call
        0x2709 | 0x1F4E7 | 0x1F4E8 | 0x1F4E9 => 0xE218, // envelope-simple
        0x1F4EB | 0x1F4EC => 0xE216, // envelope-open
        0x1F4E2 | 0x1F4E3 => 0xE324, // megaphone
        0x1F4AC | 0x1F5EF => 0xE16C, // chat-circle-dots
        0x1F4AD => 0xE164, // chat-centered-dots
        0x270F => 0xE3AE, // pencil
        0x2712 => 0xE3AA, // pen
        0x1F4DD => 0xE34C, // note-pencil
        0x1F4CB => 0xE198, // clipboard-text
        0x1F4C4 => 0xE23A, // file-text
        0x1F4C1 => 0xE25A, // folder-simple
        0x1F4C2 => 0xE260, // folders
        0x1F4C5 | 0x1F4C6 => 0xE108, // calendar
        0x1F5D3 => 0xE10A, // calendar-blank
        0x1F4CE => 0xE39A, // paperclip
        0x2702 => 0xEAE0, // scissors
        0x1F4CC | 0x1F4CD => 0xE316, // map-pin
        0x1F5D1 => 0xE4A6, // trash
        0x1F517 => 0xE2E2, // link
        0x1F514 => 0xE0CE, // bell
        0x1F515 => 0xE0D4, // bell-slash
        0x1F4D4 | 0x1F4D5 | 0x1F4D7 => 0xE0E2, // book
        0x1F4D6 => 0xE0E6, // book-open
        0x1F4DA => 0xE758, // books
        0x1F4D2 | 0x1F4D3 => 0xE34E, // notebook
        0x1F44D => 0xE48E, // thumbs-up
        0x1F44E => 0xE48C, // thumbs-down
        0x1F44B | 0x1F64B => 0xE580, // hand-waving
        0x1F44F => 0xE6A0, // hands-clapping
        0x1F64F => 0xECC8, // hands-praying
        0x1F91D => 0xE582, // handshake
        0x261D | 0x1F446 | 0x1F447 | 0x1F448 | 0x1F449 => 0xE29A, // hand-pointing
        0x270B | 0x1F590 | 0x1F91A => 0xE298, // hand
        0x270C | 0x1F91E => 0xE7CC, // hand-peace
        0x270A | 0x1F44A | 0x1F91B | 0x1F91C => 0xE57A, // hand-fist
        0x1F9D1 | 0x1F464 | 0x1F466 | 0x1F467 | 0x1F468 | 0x1F469 | 0x1F474 | 0x1F475 => 0xE4C2, // user
        0x1F465 => 0xE4D6, // users
        0x1F476 => 0xE774, // baby
        0x1F440 | 0x1F441 => 0xE220, // eye
        0x1F442 => 0xE70C, // ear
        0x1F436 | 0x1F415 => 0xE74A, // dog
        0x1F431 | 0x1F408 => 0xE748, // cat
        0x1F42D | 0x1F401 => 0xE644, // mouse-simple
        0x1F430 | 0x1F407 => 0xEAC2, // rabbit
        0x1F423 | 0x1F424 | 0x1F425 | 0x1F426 => 0xE72C, // bird
        0x1F434 | 0x1F40E => 0xE2B0, // horse
        0x1F42E | 0x1F404 => 0xEABE, // cow
        0x1F41F | 0x1F420 | 0x1F421 => 0xE728, // fish
        0x1F41B | 0x1F41C => 0xE5F4, // bug
        0x1F98B => 0xEA6E, // butterfly
        0x1F43E => 0xE648, // paw-print
        0x2615 => 0xE1C2, // coffee
        0x1F382 | 0x1F370 => 0xE780, // cake
        0x1F36A => 0xE6CA, // cookie
        0x1F366 | 0x1F368 => 0xE804, // ice-cream
        0x1F354 => 0xE790, // hamburger
        0x1F355 => 0xE796, // pizza
        0x1F37F => 0xEB4E, // popcorn
        0x1F37A | 0x1F37B => 0xEB62, // beer-stein
        0x1F377 => 0xE6B2, // wine
        0x1F3E0 | 0x1F3E1 => 0xE2C2, // house
        0x1F3E2 | 0x1F3D8 | 0x1F3E3 => 0xE102, // buildings
        0x1F3E5 => 0xE844, // hospital
        0x1F3E6 => 0xE0B4, // bank
        0x1F3EA => 0xE470, // storefront
        0x1F393 | 0x1F3EB => 0xE62C, // graduation-cap
        0x1F3ED => 0xE760, // factory
        0x26F0 => 0xE7AE, // mountains
        0x26FA => 0xE8BA, // tent
        0x1F697 | 0x1F698 => 0xE112, // car
        0x1F68C | 0x1F68D => 0xE106, // bus
        0x1F6B2 => 0xE0D6, // bicycle
        0x2708 => 0xE002, // airplane
        0x1F6EB => 0xE504, // airplane-takeoff
        0x1F6EC => 0xE502, // airplane-landing
        0x1F680 => 0xE3FC, // rocket
        0x26F5 => 0xE78A, // sailboat
        0x2693 => 0xE514, // anchor
        0x1F683 | 0x1F685 | 0x1F686 => 0xE496, // train
        0x26BD => 0xE716, // soccer-ball
        0x26BE => 0xE71A, // baseball
        0x1F3C0 => 0xE724, // basketball
        0x1F3BE => 0xE720, // tennis-ball
        0x25B6 => 0xE3D0, // play
        0x23F8 | 0x23EF => 0xE39E, // pause
        0x23F9 | 0x23FA => 0xE46C, // stop
        0x23E9 | 0x23ED => 0xE6A6, // fast-forward
        0x23EA | 0x23EE => 0xE6A8, // rewind
        0x1F500 => 0xE422, // shuffle
        0x1F501 | 0x1F503 | 0x1F504 => 0xE094, // arrows-clockwise
        0x1F3B5 => 0xE33C, // music-note
        0x1F3B6 | 0x1F3BC => 0xE340, // music-notes
        0x1F3AC => 0xE8C2, // film-slate
        0x2713 | 0x2714 => 0xE182, // check
        0x2705 | 0x2611 => 0xE184, // check-circle
        0x2716 | 0x2717 | 0x2718 | 0x274C | 0x274E => 0xE4F6, // x
        0x26A0 => 0xE4E0, // warning
        0x26D4 => 0xE3DE, // prohibit
        0x267B => 0xE75A, // recycle
        0x2139 => 0xE2CE, // info
        0x2753 | 0x2754 | 0x2755 => 0xE3E8, // question
        0x2757 | 0x203C | 0x2049 => 0xEE44, // exclamation-mark
        0x26FD => 0xE768, // gas-pump
        0x1F9ED => 0xE1C8, // compass
        0x1F5FA => 0xE31A, // map-trifold
        0x27A1 => 0xE06C, // arrow-right
        0x2B05 => 0xE058, // arrow-left
        0x2B06 => 0xE08E, // arrow-up
        0x2B07 => 0xE03E, // arrow-down
        0x2194 => 0xEB06, // arrows-horizontal
        0x2195 => 0xEB04, // arrows-vertical
        0x2196 => 0xE090, // arrow-up-left
        0x2197 => 0xE092, // arrow-up-right
        0x2198 => 0xE042, // arrow-down-right
        0x2199 => 0xE040, // arrow-down-left
        0x21A9 => 0xE08A, // arrow-u-up-left
        0x21AA => 0xE08C, // arrow-u-up-right
        0x2934 => 0xE026, // arrow-bend-up-right
        0x2935 => 0xE01A, // arrow-bend-down-right
        _ => return None,
    };
    Some(icon)
}

// 映射覆盖 359 个 emoji 码点 → 210 个 Phosphor 图标。
