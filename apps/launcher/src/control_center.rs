// KunxOS 控制中心（no_std / 无堆 / 固定内存）。
//
// 手势（与通知中心严格区分，见 cc_handle）：
//   - 打开：从右上角（顶部 50px x 右侧 50px）向下滑动 > 80px
//   - 关闭：从底部上滑 / 面板内下拉 / 点击面板外空白
//   - 通知中心（另行实现，本文件仅含手势路由与最小占位面板）：
//     从左上角（x < 120, y < 50）向下滑动 > 80px
//
// 面板：顶部滑出，高度 = 屏幕 50%，底部圆角 24px；毛玻璃背景
// （降采样盒式模糊 ≈30px + 浅色着色，玻璃透明度 0.7）；
// 展开 250ms 弹簧，收起 200ms ease-out；交互期间按 120Hz 帧预算刷新
// （动画/拖动全部基于真实时间 dt，帧率无关）。

use super::*;

/// 阻尼谐振子弹簧（半隐式欧拉积分，与仓库运动规范一致；帧率无关）。
struct CcSpring {
    stiffness: f32,
    damping: f32,
    mass: f32,
    target: f32,
    pos: f32,
    vel: f32,
}

impl CcSpring {
    const fn fixed() -> CcSpring {
        CcSpring {
            stiffness: 0.0,
            damping: 0.0,
            mass: 1.0,
            target: 0.0,
            pos: 0.0,
            vel: 0.0,
        }
    }
    /// 推进 `ms` 毫秒，返回是否已稳定在目标附近。
    fn step_ms(&mut self, ms: f32) -> bool {
        // 参数防护（与内核 anim::Spring / shared/motion.rs 一致）：mass ≥ 0.1、
        // stiffness ≤ 20000（半隐式欧拉 ωh < 2），积分出非有限值强制收敛。
        if !self.mass.is_finite() || self.mass <= 0.0 {
            self.mass = 1.0;
        } else if self.mass < 0.1 {
            self.mass = 0.1;
        }
        if !self.stiffness.is_finite() || self.stiffness < 0.0 {
            self.stiffness = 0.0;
        } else if self.stiffness > 20_000.0 {
            self.stiffness = 20_000.0;
        }
        if !self.damping.is_finite() || self.damping < 0.0 {
            self.damping = 0.0;
        }
        let mut t = ms / 1000.0;
        while t > 0.0 {
            let h = if t > 0.001 { 0.001 } else { t };
            let a =
                (-self.stiffness * (self.pos - self.target) - self.damping * self.vel) / self.mass;
            self.vel += a * h;
            self.pos += self.vel * h;
            t -= h;
            if !self.pos.is_finite() || !self.vel.is_finite() {
                self.pos = self.target;
                self.vel = 0.0;
                return true;
            }
        }
        (self.pos - self.target).abs() < 0.5 && self.vel.abs() < 1.0
    }
}

// ==================== 面板几何 ====================

const CC_PANEL_H: i32 = DISPLAY_H as i32 / 2; // 480 = 屏幕高度 50%
const CC_RADIUS: i32 = 24; // 底部圆角
const CC_GLASS_ALPHA: u32 = 178; // 0.7 * 255
const CC_DOWN: usize = 6; // 毛玻璃降采样倍率
const CC_SMALL_W: usize = (DISPLAY_W + CC_DOWN - 1) / CC_DOWN; // 90
const CC_SMALL_H: usize = (CC_PANEL_H as usize + CC_DOWN - 1) / CC_DOWN; // 80
const CC_BLUR_R: i32 = 3; // 小缓冲盒式模糊半径（≈30px）

// 布局
const CC_MX: i32 = 24;
const CC_ROW1_Y: i32 = 48; // 2x2 大开关（飞行/WiFi/蓝牙/蜂窝）
const CC_BIG: i32 = 60;
const CC_BIG_R: i32 = 16;
const CC_ROW1_GAP: i32 = 28;
const CC_ROW2_Y: i32 = 120; // 2x2 中等开关（手电筒/计时器/计算器/相机）
const CC_MED: i32 = 50;
const CC_MED_R: i32 = 12;
const CC_ROW2_GAP: i32 = 30;
const CC_BR_Y: i32 = 184; // 亮度滑块
const CC_VOL_Y: i32 = 236; // 音量滑块
const CC_SL_H: i32 = 40;
const CC_MUS_Y: i32 = 300; // 音乐控制
const CC_MUS_W: i32 = 64;
const CC_MUS_H: i32 = 56;
const CC_MUS_GAP: i32 = 24;
const CC_SHORT_Y: i32 = 380; // 快捷入口（设置/锁屏/截图）
const CC_SHORT_W: i32 = 150;
const CC_SHORT_H: i32 = 48;
const CC_SHORT_GAP: i32 = 15;

const CC_OPEN_MIN_DIST: i32 = 80; // 打开：向下滑动 > 80px
const CC_CLOSE_UP_DIST: i32 = 30; // 关闭：上滑
const CC_CLOSE_DOWN_DIST: i32 = 80; // 关闭：面板内下拉

// ==================== 状态 ====================

#[derive(Clone, Copy, PartialEq)]
enum CcPhase {
    Closed,
    Opening,
    Open,
    Closing,
    /// 跟手下拉中：进度 1:1 由手指驱动（ColorOS/鸿蒙 面板跟手语义），
    /// 抬手后按位移/速度结算为 Opening（弹簧续接）或 Closing。
    Drag,
}

static mut CC_PHASE: CcPhase = CcPhase::Closed;
static mut CC_PROG: f32 = 0.0;
static mut CC_SPRING: CcSpring = CcSpring::fixed();
static mut CC_SPRING_T: u64 = 0;
/// 收起起始进度：从打开进行中的当前进度收起（否则面板先闪跳到全开）。
static mut CC_GLASS_READY: bool = false;
static mut CC_GLASS: [u32; DISPLAY_W * (CC_PANEL_H as usize)] =
    [0; DISPLAY_W * (CC_PANEL_H as usize)];
static mut CC_SMALL: [u32; CC_SMALL_W * CC_SMALL_H] = [0; CC_SMALL_W * CC_SMALL_H];
static mut CC_TMP: [u32; CC_SMALL_W * CC_SMALL_H] = [0; CC_SMALL_W * CC_SMALL_H];

// 开关状态：0 飞行模式 / 1 WiFi / 2 蓝牙 / 3 蜂窝数据
static mut CC_SW: [bool; 4] = [false; 4];
static mut CC_FLASHLIGHT: bool = false;
static mut CC_BRIGHTNESS: u32 = 85;
static mut CC_VOLUME: u32 = 60;

/// 音量键调节（P0-4 音量 HUD 接线）：调节并落盘，返回新值。
pub unsafe fn volume_bump(delta: i32) -> u32 {
    CC_VOLUME = (CC_VOLUME as i32 + delta).clamp(0, 100) as u32;
    save_number("/data/oobe/volume", CC_VOLUME);
    CC_VOLUME
}

/// 当前音量（0..100，控制中心滑条同源）。
pub fn volume_value() -> u32 {
    unsafe { CC_VOLUME }
}
static mut CC_PLAYING: bool = false;
static mut CC_TRACK: usize = 0;
static CC_TRACKS: [&[u8]; 3] = [b"Sunset Drive", b"Neon Nights", b"Midnight Pulse"];

// 拖动 / 按压反馈
static mut CC_DRAG_SLIDER: i32 = -1; // 0 亮度 / 1 音量 / -1 无
static mut CC_DRAG_MOVED: bool = false;
static mut CC_PRESS_KIND: u8 = 0; // 0 无 / 1 开关 / 2 音乐 / 3 快捷入口
static mut CC_PRESS_IDX: u8 = 0;
static mut CC_PRESS_T0: u64 = 0;
static mut CC_PRESS_SCALE: f32 = 1.0;
static mut CC_PRESS_SPRING: CcSpring = CcSpring::fixed();
static mut CC_PRESS_SPRINGING: bool = false;

// 滑条百分比气泡（拖动亮度/音量滑条时在其上方显示数值，松手后 900ms 淡出）。
static mut CC_POP_SLIDER: i32 = -1; // 0 亮度 / 1 音量 / -1 无
static mut CC_POP_UNTIL: u64 = 0;

// 通知中心（另行实现；本文件只做手势路由与最小占位面板）
static mut NOTIF_OPEN: bool = false;
static mut NOTIF_ANIM: bool = false;
static mut NOTIF_PROG: f32 = 0.0;
static mut NOTIF_T0: u64 = 0;

// ==================== 跟手下拉（ColorOS/鸿蒙 面板跟手语义） ====================
//
// 顶部区域按下并纵向拖动 → 面板进度 1:1 跟随手指（跟手期间弹簧停摆）；
// 抬手按「位移 >30% 或 速度 >600px/s」结算为打开（弹簧从当前进度与速度
// 无缝续接，鸿蒙 springMotion 打断接续语义）或回落。结算后吞掉紧随的
// 一次手势（抬起产生的 Swipe/Tap），避免与 flick 开合路径双触发。

/// 控制中心跟手中。
static mut CC_SHADE_DRAG: bool = false;
/// 通知中心跟手中。
static mut NOTIF_SHADE_DRAG: bool = false;
/// 下拉速度低通估计（px/s，向下为正）——松手 fling 判定用。
static mut SHADE_VEL: f32 = 0.0;
static mut SHADE_LAST_DY: i32 = 0;
static mut SHADE_LAST_T: u64 = 0;
/// 跟手已结算：下一次到达 cc_handle 的手势（抬手 Swipe）只消费不执行。
static mut SHADE_GESTURE_EATEN: bool = false;
/// 抬手手势已被「跟手中」守卫消费（shade_drag_release 据此不再重复吞）。
static mut SHADE_ATE_RELEASE: bool = false;

/// 跟手下拉速度阈值（px/s）：超过即视为甩动打开。
const SHADE_FLING_VEL: f32 = 600.0;
/// 跟手结算位移阈值：面板进度超过 30% 松手打开。
const SHADE_OPEN_PROG: f32 = 0.3;

/// 跟手下拉是否可用（面板均关闭且无其它全屏语义占用）。
pub fn shade_drag_allowed() -> bool {
    unsafe {
        !LOCKED
            && FOLDER_PHASE == FolderPhase::Closed
            && !RECENTS_OPEN
            && !EDIT_MODE
            && !RENAME_OPEN
            && !PAGE_DRAG
            && !PAGE_ANIM
            && !BOOT_ACTIVE
            && CC_PHASE == CcPhase::Closed
            && CC_PROG <= 0.001
            && NOTIF_PROG <= 0.001
            && matches!(MOTION.phase, Phase::Idle | Phase::PressIn)
    }
}

/// 跟手是否进行中（主循环判定重绘 / 抬手结算 / 翻页互斥）。
pub fn shade_drag_active() -> bool {
    unsafe { CC_SHADE_DRAG || NOTIF_SHADE_DRAG }
}

/// 开始跟手下拉：`left` = 通知中心（左上角），否则控制中心（右上角）。
/// 返回是否成功接管本次手势。
pub fn shade_drag_start(left: bool) -> bool {
    unsafe {
        if !shade_drag_allowed() {
            return false;
        }
        SHADE_VEL = 0.0;
        SHADE_LAST_DY = 0;
        SHADE_LAST_T = now_us();
        if left {
            NOTIF_SHADE_DRAG = true;
            // 跟手期间动画停摆，进度由 shade_drag_update 直接驱动。
            NOTIF_OPEN = true;
            NOTIF_ANIM = false;
            NOTIF_PROG = 0.0;
        } else {
            CC_SHADE_DRAG = true;
            CC_PHASE = CcPhase::Drag;
            CC_PROG = 0.0;
            // 跟手打开与 flick 打开一致：重建玻璃背景（否则 backdrop 是上
            // 一次打开时缓存的旧桌面快照，与当前壁纸不符）。
            CC_GLASS_READY = false;
        }
        NEEDS_REDRAW = true;
        true
    }
}

/// 跟手推进：面板进度 = 下拉位移 / 打开距离（1:1，钳制 0..1），
/// 同步低通估计下拉速度供松手 fling 判定。
pub fn shade_drag_update(dy: i32) {
    unsafe {
        let now = now_us();
        let dt = now.saturating_sub(SHADE_LAST_T) as f32 / 1_000_000.0;
        if dt > 0.0 && dt < 0.5 {
            // 事件级抖动大，0.6/0.4 低通平滑出可用的 fling 速度。
            // 钳制：dt 下限 1µs 时瞬时速度可达 10⁶ px/s，一路透传给弹簧
            // 初速会把面板进度一帧冲过 1（位移异常 + 误判 fling）。
            let v = ((dy - SHADE_LAST_DY) as f32 / dt).clamp(-3000.0, 3000.0);
            SHADE_VEL = (SHADE_VEL * 0.6 + v * 0.4).clamp(-3000.0, 3000.0);
        }
        SHADE_LAST_DY = dy;
        SHADE_LAST_T = now;
        let p = (dy as f32 / CC_OPEN_MIN_DIST as f32).clamp(0.0, 1.0);
        if NOTIF_SHADE_DRAG {
            NOTIF_PROG = p;
        } else {
            CC_PROG = p;
        }
        NEEDS_REDRAW = true;
    }
}

/// 抬手结算：进度 >30% 或速度 >600px/s → 打开（弹簧/动画从当前进度
/// 与速度续接）；否则回落。结算后吞掉紧随的一次手势。
pub fn shade_drag_release() {
    unsafe {
        let cc = CC_SHADE_DRAG;
        let notif = NOTIF_SHADE_DRAG;
        CC_SHADE_DRAG = false;
        NOTIF_SHADE_DRAG = false;
        // 抬手手势若已在 cc_handle 被消费则不重复吞；否则吞掉紧随的手势。
        if SHADE_ATE_RELEASE {
            SHADE_ATE_RELEASE = false;
        } else {
            SHADE_GESTURE_EATEN = true;
        }
        if !cc && !notif {
            return;
        }
        let p = if cc { CC_PROG } else { NOTIF_PROG };
        let open = p > SHADE_OPEN_PROG || SHADE_VEL > SHADE_FLING_VEL;
        if cc {
            if open {
                // 弹簧从当前进度起跳并继承手指速度（进度/秒），无缝续接。
                CC_PHASE = CcPhase::Opening;
                CC_SPRING = CcSpring {
                    stiffness: 420.0,
                    damping: 2.0 * fsqrt(420.0) * 0.85,
                    mass: 1.0,
                    target: 1.0,
                    pos: CC_PROG,
                    vel: (SHADE_VEL / CC_PANEL_H as f32).clamp(-4.0, 4.0),
                };
                CC_SPRING_T = now_us();
            } else {
                CC_PHASE = CcPhase::Closing;
                CC_SPRING = CcSpring {
                    stiffness: 420.0,
                    damping: 2.0 * fsqrt(420.0) * 0.9,
                    mass: 1.0,
                    target: 0.0,
                    pos: CC_PROG,
                    // 继承手指速度（收起方向的负速度同样接续）
                    vel: (SHADE_VEL / CC_PANEL_H as f32).clamp(-4.0, 4.0),
                };
                CC_SPRING_T = now_us();
            }
        } else {
            NOTIF_OPEN = open;
            NOTIF_ANIM = true;
            // 时间偏移让 200ms EaseOut 从当前进度无缝续播（不闪跳回 0）。
            let off = if open { NOTIF_PROG } else { 1.0 - NOTIF_PROG };
            NOTIF_T0 = now_us().saturating_sub((200_000.0 * off.clamp(0.0, 1.0)) as u64);
        }
        NEEDS_REDRAW = true;
    }
}

const NOTIF_H: i32 = 360;

const WD_ZH: [&[u8]; 7] = [
    b"\xe5\x91\xa8\xe6\x97\xa5", // 周日
    b"\xe5\x91\xa8\xe4\xb8\x80", // 周一
    b"\xe5\x91\xa8\xe4\xba\x8c", // 周二
    b"\xe5\x91\xa8\xe4\xb8\x89", // 周三
    b"\xe5\x91\xa8\xe5\x9b\x9b", // 周四
    b"\xe5\x91\xa8\xe4\xba\x94", // 周五
    b"\xe5\x91\xa8\xe5\x85\xad", // 周六
];
const WD_EN: [&[u8]; 7] = [b"Sun", b"Mon", b"Tue", b"Wed", b"Thu", b"Fri", b"Sat"];

// ==================== 打开 / 关闭 ====================

fn cc_open() {
    unsafe {
        if NOTIF_OPEN && !NOTIF_SHADE_DRAG {
            cc_notif_close();
        }
        if CC_PHASE == CcPhase::Open || CC_PHASE == CcPhase::Opening || CC_PHASE == CcPhase::Drag {
            return;
        }
        CC_GLASS_READY = false;
        CC_PHASE = CcPhase::Opening;
        CC_SPRING = CcSpring {
            stiffness: 420.0,
            damping: 2.0 * fsqrt(420.0) * 0.85,
            mass: 1.0,
            target: 1.0,
            pos: 0.0,
            vel: 0.0,
        };
        CC_SPRING_T = now_us();
        CC_PROG = 0.0;
        CC_DRAG_SLIDER = -1;
        CC_PRESS_KIND = 0;
        CC_PRESS_SPRINGING = false;
        CC_PRESS_SCALE = 1.0;
        CC_BRIGHTNESS = BRIGHTNESS;
        NEEDS_REDRAW = true;
    }
}

fn cc_close() {
    unsafe {
        if CC_PHASE == CcPhase::Closed || CC_PHASE == CcPhase::Closing {
            return;
        }
        CC_PHASE = CcPhase::Closing;
        // 非手势路径（点外部/按钮关闭）：从当前进度零初速弹簧回零
        CC_SPRING = CcSpring {
            stiffness: 420.0,
            damping: 2.0 * fsqrt(420.0) * 0.9,
            mass: 1.0,
            target: 0.0,
            pos: CC_PROG,
            vel: 0.0,
        };
        CC_SPRING_T = now_us();
        CC_DRAG_SLIDER = -1;
        CC_PRESS_KIND = 0;
        CC_PRESS_SPRINGING = false;
        CC_PRESS_SCALE = 1.0;
        NEEDS_REDRAW = true;
    }
}

pub fn cc_is_open() -> bool {
    unsafe { CC_PHASE == CcPhase::Open }
}

/// 面板是否可见（打开中 / 打开 / 收起的动画全程视为可见，消费输入）。
pub fn cc_visible() -> bool {
    unsafe { CC_PROG > 0.001 }
}

/// 通知中心是否可见（含开合动画全程）。主循环据此抑制状态栏带状局部
/// 重绘（B12 充电呼吸节拍）—— 通知面板覆盖状态栏区域时，带状重绘会把
/// 状态栏画到面板上方。
pub fn notif_visible() -> bool {
    unsafe { NOTIF_OPEN || NOTIF_PROG > 0.001 }
}

fn cc_panel_top() -> f32 {
    unsafe { CC_PANEL_H as f32 * (CC_PROG - 1.0) }
}

// ==================== 通知中心（最小占位，另行实现） ====================

fn cc_notif_open() {
    unsafe {
        if CC_PHASE != CcPhase::Closed {
            cc_close();
        }
        NOTIF_OPEN = true;
        NOTIF_ANIM = true;
        NOTIF_T0 = now_us();
        NEEDS_REDRAW = true;
    }
}

fn cc_notif_close() {
    unsafe {
        NOTIF_OPEN = false;
        NOTIF_ANIM = true;
        NOTIF_T0 = now_us();
        NEEDS_REDRAW = true;
    }
}

fn cc_notif_advance(now: u64) -> bool {
    unsafe {
        if !NOTIF_ANIM {
            return false;
        }
        let t = now.saturating_sub(NOTIF_T0) as f32 / 200_000.0;
        if t >= 1.0 {
            NOTIF_ANIM = false;
            NOTIF_PROG = if NOTIF_OPEN { 1.0 } else { 0.0 };
            NEEDS_REDRAW = true;
            return false;
        }
        let e = Easing::EaseOut.sample(t);
        NOTIF_PROG = if NOTIF_OPEN { e } else { 1.0 - e };
        true
    }
}

// ==================== 手势（含与通知中心的区分） ====================

pub fn cc_handle(g: Gesture) -> bool {
    unsafe {
        // 跟手下拉刚结算：抬手产生的 Swipe/Tap 只吞掉不执行，
        // 否则 flick 开合路径会与跟手结算双触发（如中途抬起又闪开）。
        if SHADE_GESTURE_EATEN {
            SHADE_GESTURE_EATEN = false;
            return true;
        }
        // 跟手进行中：手势一律消费（结算由主循环抬手兜底统一处理）；
        // 抬手产生的 Swipe/Tap 到达于此，登记避免 release 再吞一次。
        if shade_drag_active() {
            SHADE_ATE_RELEASE = true;
            return true;
        }
        if CC_DRAG_MOVED {
            // 滑块拖动产生的手势：只跟手，不触发任何点击动作
            CC_DRAG_MOVED = false;
            return true;
        }
        match g {
            Gesture::Tap { x, y } => {
                if cc_visible() {
                    return cc_tap(x, y);
                }
                if NOTIF_PROG > 0.001 {
                    cc_notif_close();
                    return true;
                }
                false
            }
            Gesture::Swipe { dx, dy, .. } => {
                if cc_visible() {
                    if dy < -CC_CLOSE_UP_DIST || dy > CC_CLOSE_DOWN_DIST {
                        cc_close();
                    }
                    return true;
                }
                if NOTIF_PROG > 0.001 {
                    if dy < -CC_CLOSE_UP_DIST || dy > CC_CLOSE_DOWN_DIST {
                        cc_notif_close();
                    }
                    return true;
                }
                // 面板均关闭：按起点区分通知中心（左上）与控制中心（右上）
                if dy > CC_OPEN_MIN_DIST && dy.abs() > dx.abs() {
                    let (px, py) = (TOUCH.press_x, TOUCH.press_y);
                    if py <= 50 && px >= DISPLAY_W as i32 - 50 {
                        cc_open();
                        return true;
                    }
                    if py <= 50 && px < 120 {
                        cc_notif_open();
                        return true;
                    }
                }
                false
            }
            Gesture::Nav(_) => {
                if cc_visible() {
                    cc_close();
                    return true;
                }
                if NOTIF_PROG > 0.001 {
                    cc_notif_close();
                    return true;
                }
                false
            }
            Gesture::LongPress { .. } => {
                if cc_visible() || NOTIF_PROG > 0.001 {
                    return true;
                }
                false
            }
            Gesture::None => false,
        }
    }
}

// ==================== 点击 ====================

#[derive(Clone, Copy, PartialEq)]
enum CcEl {
    None,
    Tile(u8),   // 0..7
    Slider(u8), // 0 亮度 / 1 音量
    Music(u8),  // 0 上一首 / 1 播放 / 2 下一首
    Short(u8),  // 0 设置 / 1 锁屏 / 2 截图
}

fn cc_row1_x(i: usize) -> i32 {
    let total = 4 * CC_BIG + 3 * CC_ROW1_GAP;
    let x0 = (DISPLAY_W as i32 - total) / 2;
    x0 + i as i32 * (CC_BIG + CC_ROW1_GAP)
}

fn cc_row2_x(i: usize) -> i32 {
    cc_row1_x(i) + (CC_BIG - CC_MED) / 2
}

fn cc_hit(x: i32, y: i32) -> CcEl {
    for i in 0..4usize {
        let tx = cc_row1_x(i);
        if x >= tx && x < tx + CC_BIG && y >= CC_ROW1_Y && y < CC_ROW1_Y + CC_BIG {
            return CcEl::Tile(i as u8);
        }
    }
    for i in 0..4usize {
        let tx = cc_row2_x(i);
        if x >= tx && x < tx + CC_MED && y >= CC_ROW2_Y && y < CC_ROW2_Y + CC_MED {
            return CcEl::Tile(4 + i as u8);
        }
    }
    if x >= CC_MX && x < DISPLAY_W as i32 - CC_MX && y >= CC_BR_Y && y < CC_BR_Y + CC_SL_H {
        return CcEl::Slider(0);
    }
    if x >= CC_MX && x < DISPLAY_W as i32 - CC_MX && y >= CC_VOL_Y && y < CC_VOL_Y + CC_SL_H {
        return CcEl::Slider(1);
    }
    if y >= CC_MUS_Y && y < CC_MUS_Y + CC_MUS_H {
        let x0 = cc_mus_x0();
        for i in 0..3usize {
            let bx = x0 + i as i32 * (CC_MUS_W + CC_MUS_GAP);
            if x >= bx && x < bx + CC_MUS_W {
                return CcEl::Music(i as u8);
            }
        }
    }
    if y >= CC_SHORT_Y && y < CC_SHORT_Y + CC_SHORT_H {
        let x0 = cc_short_x0();
        for i in 0..3usize {
            let sx = x0 + i as i32 * (CC_SHORT_W + CC_SHORT_GAP);
            if x >= sx && x < sx + CC_SHORT_W {
                return CcEl::Short(i as u8);
            }
        }
    }
    CcEl::None
}

fn cc_tap(x: i32, y: i32) -> bool {
    unsafe {
        if CC_PHASE != CcPhase::Open {
            return true;
        }
        if y >= CC_PANEL_H || y < 0 {
            cc_close();
            return true;
        }
        match cc_hit(x, y) {
            CcEl::Tile(i) => {
                cc_tile_tap(i as usize);
                true
            }
            CcEl::Slider(s) => {
                cc_set_slider(s as usize, x);
                true
            }
            CcEl::Music(m) => {
                cc_music_tap(m as usize);
                true
            }
            CcEl::Short(s) => {
                cc_short_tap(s as usize);
                true
            }
            CcEl::None => true,
        }
    }
}

fn cc_tile_tap(i: usize) {
    unsafe {
        match i {
            0 => {
                CC_SW[0] = !CC_SW[0];
                if CC_SW[0] {
                    CC_SW[1] = false;
                    CC_SW[2] = false;
                    CC_SW[3] = false;
                    WIFI_ENABLED = false;
                    save_number(WIFI_PATH, 0);
                }
                cc_save_switches();
                show_toast(txt(
                    if CC_SW[0] {
                        "Airplane mode ON"
                    } else {
                        "Airplane mode OFF"
                    },
                    if CC_SW[0] {
                        "飞行模式已开启"
                    } else {
                        "飞行模式已关闭"
                    },
                ));
            }
            1 => {
                WIFI_ENABLED = !WIFI_ENABLED;
                CC_SW[1] = WIFI_ENABLED;
                save_number(WIFI_PATH, if WIFI_ENABLED { 1 } else { 0 });
                cc_save_switches();
                show_toast(txt(
                    if WIFI_ENABLED {
                        "Wi-Fi ON"
                    } else {
                        "Wi-Fi OFF"
                    },
                    if WIFI_ENABLED {
                        "无线已开启"
                    } else {
                        "无线已关闭"
                    },
                ));
            }
            2 => {
                CC_SW[2] = !CC_SW[2];
                cc_save_switches();
                show_toast(txt(
                    if CC_SW[2] {
                        "Bluetooth ON"
                    } else {
                        "Bluetooth OFF"
                    },
                    if CC_SW[2] {
                        "蓝牙已开启"
                    } else {
                        "蓝牙已关闭"
                    },
                ));
            }
            3 => {
                CC_SW[3] = !CC_SW[3];
                cc_save_switches();
                show_toast(txt(
                    if CC_SW[3] {
                        "Cellular ON"
                    } else {
                        "Cellular OFF"
                    },
                    if CC_SW[3] {
                        "蜂窝数据已开启"
                    } else {
                        "蜂窝数据已关闭"
                    },
                ));
            }
            4 => {
                CC_FLASHLIGHT = !CC_FLASHLIGHT;
                show_toast(txt(
                    if CC_FLASHLIGHT {
                        "Flashlight ON"
                    } else {
                        "Flashlight OFF"
                    },
                    if CC_FLASHLIGHT {
                        "手电筒已开启"
                    } else {
                        "手电筒已关闭"
                    },
                ));
            }
            5 => {
                let idx = cc_app_idx("clock");
                if idx < APPS.len() {
                    cc_close();
                    start_launch(idx);
                }
            }
            6 => {
                let idx = cc_app_idx("calculator");
                if idx < APPS.len() {
                    cc_close();
                    start_launch(idx);
                }
            }
            _ => {
                let idx = cc_app_idx("camera");
                if idx < APPS.len() {
                    cc_close();
                    start_launch(idx);
                }
            }
        }
        NEEDS_REDRAW = true;
    }
}

fn cc_music_tap(m: usize) {
    unsafe {
        match m {
            0 => CC_TRACK = (CC_TRACK + 2) % CC_TRACKS.len(),
            1 => CC_PLAYING = !CC_PLAYING,
            _ => CC_TRACK = (CC_TRACK + 1) % CC_TRACKS.len(),
        }
        if m != 1 {
            show_toast(CC_TRACKS[CC_TRACK]);
        } else {
            show_toast(txt(
                if CC_PLAYING { "Playing" } else { "Paused" },
                if CC_PLAYING {
                    "正在播放"
                } else {
                    "已暂停"
                },
            ));
        }
        NEEDS_REDRAW = true;
    }
}

fn cc_short_tap(s: usize) {
    unsafe {
        match s {
            0 => {
                let idx = cc_app_idx("settings");
                if idx < APPS.len() {
                    cc_close();
                    start_launch(idx);
                }
            }
            1 => lock_now(),
            _ => cc_screenshot(),
        }
        NEEDS_REDRAW = true;
    }
}

fn cc_app_idx(key: &str) -> usize {
    for i in 0..APPS.len() {
        if APPS[i].key == key {
            return i;
        }
    }
    usize::MAX
}

fn cc_save_switches() {
    unsafe {
        let mut mask = 0u32;
        if CC_SW[0] {
            mask |= 1;
        }
        if CC_SW[1] {
            mask |= 2;
        }
        if CC_SW[2] {
            mask |= 4;
        }
        if CC_SW[3] {
            mask |= 8;
        }
        save_number("/data/oobe/cc_switches", mask);
    }
}

pub fn cc_load() {
    unsafe {
        CC_BRIGHTNESS = BRIGHTNESS;
        let mut buf = [0u8; 8];
        let n = read_file("/data/oobe/cc_switches", &mut buf);
        if n > 0 {
            let mask = read_number("/data/oobe/cc_switches", 0);
            CC_SW[0] = mask & 1 != 0;
            CC_SW[1] = mask & 2 != 0;
            CC_SW[2] = mask & 4 != 0;
            CC_SW[3] = mask & 8 != 0;
            WIFI_ENABLED = CC_SW[1];
        } else {
            CC_SW[1] = WIFI_ENABLED;
        }
        CC_VOLUME = read_number("/data/oobe/volume", 60).clamp(0, 100);
        CC_FLASHLIGHT = false;
    }
}

// ==================== 滑块 ====================

fn cc_slider_w() -> i32 {
    DISPLAY_W as i32 - 2 * CC_MX - 46
}

fn cc_slider_fx(label: &[u8]) -> i32 {
    CC_MX + 14 + text_width(label) + 10
}

fn cc_set_slider(s: usize, px: i32) {
    unsafe {
        let label = if s == 0 {
            txt("Brightness", "亮度")
        } else {
            txt("Volume", "音量")
        };
        let x = CC_MX;
        let w = cc_slider_w();
        let fx = cc_slider_fx(label);
        let tw = w - (fx - x);
        let v = ((((px - fx) * 100) / tw).clamp(0, 100)) as u32;
        if s == 0 {
            BRIGHTNESS = v;
            CC_BRIGHTNESS = v;
        } else {
            CC_VOLUME = v;
        }
        // 落盘时机：拖动中每个输入事件都写盘（ABS 事件频率可达每秒百次
        // open/write/close），改为内存更新、cc_touch_up 结算时统一落盘。
        // 数值气泡：跟随拖动显示，松手后保持 900ms。
        CC_POP_SLIDER = s as i32;
        CC_POP_UNTIL = now_us() + 900_000;
        NEEDS_REDRAW = true;
    }
}

/// 滑条数值气泡（玻璃药丸 + 百分比，位于被拖动滑条上方居中）。
unsafe fn cc_slider_pop(oy: i32) {
    let now = now_us();
    if now >= CC_POP_UNTIL {
        return;
    }
    let s = CC_POP_SLIDER;
    if s != 0 && s != 1 {
        return;
    }
    let v = if s == 0 { BRIGHTNESS } else { CC_VOLUME };
    let y = if s == 0 { CC_BR_Y } else { CC_VOL_Y } + oy;
    let cx = DISPLAY_W as i32 / 2;
    draw_glass(cx - 34, y - 44, 68, 30, 15);
    let mut buf = [0u8; 5];
    let n = push_dec(&mut buf, v);
    buf[n] = b'%';
    let tw = text_width(&buf[..n + 1]);
    draw_text(cx - tw / 2, y - 36, &buf[..n + 1], COLOR_TEXT_MAIN);
}

// ==================== 截图 ====================

fn cc_screenshot() {
    unsafe {
        let fd = open("/data/screenshot.ppm", O_WRONLY | O_CREAT | O_TRUNC);
        if fd < 0 {
            show_toast(txt("Screenshot failed", "截图失败"));
            return;
        }
        let hdr = b"P6\n540 960\n255\n";
        if write_fd(fd as usize, hdr) != hdr.len() {
            syscall1(SYS_CLOSE, fd as usize);
            show_toast(txt("Screenshot failed", "截图失败"));
            return;
        }
        static mut SHOT_BUF: [u8; 4096] = [0u8; 4096];
        let mut off = 0usize;
        while off < DISPLAY_W * DISPLAY_H {
            let mut m = 0usize;
            while m + 3 <= SHOT_BUF.len() && off < DISPLAY_W * DISPLAY_H {
                let p = BACK_BUF[off];
                SHOT_BUF[m] = ((p >> 16) & 0xFF) as u8;
                SHOT_BUF[m + 1] = ((p >> 8) & 0xFF) as u8;
                SHOT_BUF[m + 2] = (p & 0xFF) as u8;
                m += 3;
                off += 1;
            }
            let w = write_fd(fd as usize, &SHOT_BUF[..m]);
            if w != m {
                break;
            }
        }
        syscall1(SYS_CLOSE, fd as usize);
        show_toast(txt("Screenshot saved", "截图已保存"));
    }
}

// ==================== 触摸拖动（滑块跟手） ====================

pub fn cc_touch_down(x: i32, y: i32) {
    unsafe {
        if CC_PHASE != CcPhase::Open {
            return;
        }
        CC_PRESS_KIND = 0;
        CC_PRESS_SPRINGING = false;
        CC_PRESS_SCALE = 1.0;
        CC_DRAG_SLIDER = -1;
        CC_DRAG_MOVED = false;
        if y >= CC_PANEL_H || y < 0 {
            return;
        }
        match cc_hit(x, y) {
            CcEl::Slider(s) => {
                CC_DRAG_SLIDER = s as i32;
                cc_set_slider(s as usize, x);
            }
            CcEl::Tile(i) => {
                CC_PRESS_KIND = 1;
                CC_PRESS_IDX = i;
                CC_PRESS_T0 = now_us();
            }
            CcEl::Music(m) => {
                CC_PRESS_KIND = 2;
                CC_PRESS_IDX = m;
                CC_PRESS_T0 = now_us();
            }
            CcEl::Short(s) => {
                CC_PRESS_KIND = 3;
                CC_PRESS_IDX = s;
                CC_PRESS_T0 = now_us();
            }
            CcEl::None => {}
        }
        NEEDS_REDRAW = true;
    }
}

pub fn cc_drag_move(x: i32, y: i32) {
    unsafe {
        if CC_DRAG_SLIDER >= 0 {
            cc_set_slider(CC_DRAG_SLIDER as usize, x);
            CC_DRAG_MOVED = true;
        }
        let _ = y;
    }
}

pub fn cc_touch_up() -> bool {
    unsafe {
        let was_drag = CC_DRAG_SLIDER >= 0 && CC_DRAG_MOVED;
        // 拖动/点按滑轨结束：把该滑条值落盘一次（cc_set_slider 只更新内存）。
        if CC_DRAG_SLIDER == 0 {
            save_number(BRIGHTNESS_PATH, BRIGHTNESS);
        } else if CC_DRAG_SLIDER == 1 {
            save_number("/data/oobe/volume", CC_VOLUME);
        }
        if CC_PRESS_KIND != 0 {
            CC_PRESS_SPRING = CcSpring {
                stiffness: 320.0,
                damping: 2.0 * fsqrt(320.0) * 0.7,
                mass: 1.0,
                target: 1.0,
                pos: CC_PRESS_SCALE,
                vel: 0.0,
            };
            CC_PRESS_SPRINGING = true;
            CC_PRESS_T0 = now_us();
        }
        CC_DRAG_SLIDER = -1;
        CC_DRAG_MOVED = false;
        NEEDS_REDRAW = true;
        was_drag
    }
}

// ==================== 动画推进（真实 dt，帧率无关） ====================

pub fn cc_advance(now: u64) -> bool {
    unsafe {
        let mut anim = false;
        match CC_PHASE {
            CcPhase::Opening => {
                let dt = now.saturating_sub(CC_SPRING_T) as f32 / 1000.0;
                CC_SPRING_T = now;
                CC_SPRING.step_ms(dt);
                CC_PROG = CC_SPRING.pos;
                if (CC_PROG - 1.0).abs() < 0.02 && CC_SPRING.vel.abs() < 2.0 {
                    CC_PROG = 1.0;
                    CC_PHASE = CcPhase::Open;
                }
                anim = true;
            }
            CcPhase::Drag => {
                // 跟手期：进度由 shade_drag_update 直接驱动，这里只保持重绘。
                anim = true;
            }
            CcPhase::Closing => {
                // 弹簧回零（P2-4 速度接续）：松手瞬间的下滑速度被继承，
                // 与打开方向的续接对称（此前 200ms 无速度 tween——位置连续
                // 但速度跳变，快速下甩时有顿挫感）。
                let dt = now.saturating_sub(CC_SPRING_T) as f32 / 1000.0;
                CC_SPRING_T = now;
                CC_SPRING.step_ms(dt);
                CC_PROG = CC_SPRING.pos.clamp(0.0, 1.2);
                if (CC_PROG - 0.0).abs() < 0.02 && CC_SPRING.vel.abs() < 0.05 {
                    CC_PROG = 0.0;
                    CC_PHASE = CcPhase::Closed;
                    NEEDS_REDRAW = true;
                } else {
                    anim = true;
                }
            }
            _ => {}
        }
        if CC_PRESS_KIND != 0 || CC_PRESS_SPRINGING {
            if cc_press_advance(now) {
                anim = true;
            }
        }
        if cc_notif_advance(now) {
            anim = true;
        }
        // 数值气泡淡出等待期（松手后 900ms 内保持重绘直至气泡消失）。
        if now < CC_POP_UNTIL && CC_PHASE == CcPhase::Open {
            anim = true;
        }
        anim
    }
}

fn cc_press_advance(now: u64) -> bool {
    unsafe {
        let mut active = false;
        if CC_PRESS_KIND != 0 && !CC_PRESS_SPRINGING {
            let t = now.saturating_sub(CC_PRESS_T0) as f32 / 150_000.0;
            let e = if t >= 1.0 {
                1.0
            } else {
                Easing::EaseOut.sample(t)
            };
            CC_PRESS_SCALE = 1.0 - 0.1 * e;
            active = true;
        }
        if CC_PRESS_SPRINGING {
            let dt = now.saturating_sub(CC_PRESS_T0) as f32 / 1000.0;
            CC_PRESS_T0 = now;
            if CC_PRESS_SPRING.step_ms(dt) {
                CC_PRESS_SPRINGING = false;
                CC_PRESS_KIND = 0;
                CC_PRESS_SCALE = 1.0;
            } else {
                CC_PRESS_SCALE = CC_PRESS_SPRING.pos;
            }
            active = true;
        }
        active
    }
}

pub fn cc_high_refresh() -> bool {
    unsafe {
        CC_PHASE == CcPhase::Opening
            || CC_PHASE == CcPhase::Closing
            || CC_PHASE == CcPhase::Drag
            || CC_SHADE_DRAG
            || NOTIF_SHADE_DRAG
            || CC_PRESS_KIND != 0
            || CC_PRESS_SPRINGING
            || CC_DRAG_SLIDER >= 0
            || NOTIF_ANIM
    }
}

// ==================== 毛玻璃背景 ====================

fn cc_build_glass() {
    unsafe {
        // 1. 降采样面板区域
        for j in 0..CC_SMALL_H {
            let sy = (j * CC_DOWN + CC_DOWN / 2).min(CC_PANEL_H as usize - 1);
            for i in 0..CC_SMALL_W {
                let sx = (i * CC_DOWN + CC_DOWN / 2).min(DISPLAY_W - 1);
                CC_SMALL[j * CC_SMALL_W + i] = BACK_BUF[sy * DISPLAY_W + sx];
            }
        }
        // 2. 两次可分离盒式模糊（水平 + 垂直），近似 30px 高斯
        for _ in 0..2 {
            for j in 0..CC_SMALL_H {
                for i in 0..CC_SMALL_W {
                    let mut rs = 0u32;
                    let mut gs = 0u32;
                    let mut bs = 0u32;
                    let mut n = 0u32;
                    let i0 = (i as i32 - CC_BLUR_R).max(0);
                    let i1 = (i as i32 + CC_BLUR_R).min(CC_SMALL_W as i32 - 1);
                    for k in i0..=i1 {
                        let p = CC_SMALL[j * CC_SMALL_W + k as usize];
                        rs += (p >> 16) & 0xFF;
                        gs += (p >> 8) & 0xFF;
                        bs += p & 0xFF;
                        n += 1;
                    }
                    CC_TMP[j * CC_SMALL_W + i] = ((rs / n) << 16) | ((gs / n) << 8) | (bs / n);
                }
            }
            for i in 0..CC_SMALL_W {
                for j in 0..CC_SMALL_H {
                    let mut rs = 0u32;
                    let mut gs = 0u32;
                    let mut bs = 0u32;
                    let mut n = 0u32;
                    let j0 = (j as i32 - CC_BLUR_R).max(0);
                    let j1 = (j as i32 + CC_BLUR_R).min(CC_SMALL_H as i32 - 1);
                    for k in j0..=j1 {
                        let p = CC_TMP[k as usize * CC_SMALL_W + i];
                        rs += (p >> 16) & 0xFF;
                        gs += (p >> 8) & 0xFF;
                        bs += p & 0xFF;
                        n += 1;
                    }
                    CC_SMALL[j * CC_SMALL_W + i] = ((rs / n) << 16) | ((gs / n) << 8) | (bs / n);
                }
            }
        }
        // 3. 双线性放大 + 浅色着色（白色磨砂），写入玻璃层
        for y in 0..CC_PANEL_H {
            let fy = (y as f32 / CC_DOWN as f32) - 0.5;
            let j0 = cc_floor_i32(fy).max(0);
            let j1 = (j0 + 1).min(CC_SMALL_H as i32 - 1);
            let ty = fy - j0 as f32;
            for x in 0..DISPLAY_W {
                let fx = (x as f32 / CC_DOWN as f32) - 0.5;
                let i0 = cc_floor_i32(fx).max(0);
                let i1 = (i0 + 1).min(CC_SMALL_W as i32 - 1);
                let tx = fx - i0 as f32;
                let a = CC_SMALL[j0 as usize * CC_SMALL_W + i0 as usize];
                let b = CC_SMALL[j0 as usize * CC_SMALL_W + i1 as usize];
                let c = CC_SMALL[j1 as usize * CC_SMALL_W + i0 as usize];
                let d = CC_SMALL[j1 as usize * CC_SMALL_W + i1 as usize];
                let bl = cc_bilerp(a, b, c, d, tx, ty);
                CC_GLASS[y as usize * DISPLAY_W + x] = blend(bl, 0x00_FF_FF_FF, 150);
            }
        }
    }
}

/// 截断向零取整（无 std 的 f32 as i32 语义），用于坐标换算。
fn cc_floor_i32(x: f32) -> i32 {
    x as i32
}

/// 向上取整（无 std 的 f32::ceil）。
fn cc_ceil_i32(x: f32) -> i32 {
    let t = x as i32;
    if (t as f32) < x {
        t + 1
    } else {
        t
    }
}

fn cc_bilerp(a: u32, b: u32, c: u32, d: u32, tx: f32, ty: f32) -> u32 {
    let lerp = |x: u32, y: u32, t: f32| (x as f32 + (y as f32 - x as f32) * t) as u32;
    let ar = (a >> 16) & 0xFF;
    let ag = (a >> 8) & 0xFF;
    let ab = a & 0xFF;
    let br = (b >> 16) & 0xFF;
    let bg = (b >> 8) & 0xFF;
    let bb = b & 0xFF;
    let cr = (c >> 16) & 0xFF;
    let cg = (c >> 8) & 0xFF;
    let cb = c & 0xFF;
    let dr = (d >> 16) & 0xFF;
    let dg = (d >> 8) & 0xFF;
    let db = d & 0xFF;
    let tr = lerp(ar, br, tx);
    let tg = lerp(ag, bg, tx);
    let tb = lerp(ab, bb, tx);
    let br2 = lerp(cr, dr, tx);
    let bg2 = lerp(cg, dg, tx);
    let bb2 = lerp(cb, db, tx);
    let r = lerp(tr, br2, ty);
    let g = lerp(tg, bg2, ty);
    let b = lerp(tb, bb2, ty);
    (r << 16) | (g << 8) | b
}

fn cc_bottom_round(x: i32, r: i32) -> bool {
    let rad = CC_RADIUS;
    if r < CC_PANEL_H - rad {
        return true;
    }
    // 超椭圆几何与系统 glass 模块一致（ColorOS 15 连续曲率）。
    glass::in_round_rect(x, r, DISPLAY_W as i32, CC_PANEL_H, rad)
}

// ==================== 渲染 ====================

pub fn cc_draw() {
    unsafe {
        cc_draw_notif();
        if CC_PROG <= 0.001 {
            return;
        }
        if !CC_GLASS_READY {
            cc_build_glass();
            CC_GLASS_READY = true;
        }
        cc_composite();
        cc_draw_content();
    }
}

fn cc_composite() {
    unsafe {
        let y_top = cc_panel_top();
        let y0 = cc_ceil_i32(y_top);
        let yb = cc_ceil_i32(y_top + CC_PANEL_H as f32);
        if y0 >= yb {
            return;
        }
        let y1 = yb.min(DISPLAY_H as i32);
        for y in y0.max(0)..y1 {
            let r = (y as f32 - y_top) as i32;
            if r < 0 || r >= CC_PANEL_H {
                continue;
            }
            let row = y as usize * DISPLAY_W;
            let grow = r as usize * DISPLAY_W;
            for x in 0..DISPLAY_W {
                if !cc_bottom_round(x as i32, r) {
                    continue;
                }
                let bg = BACK_BUF[row + x];
                let g = CC_GLASS[grow + x];
                BACK_BUF[row + x] = blend(bg, g, CC_GLASS_ALPHA);
            }
        }
    }
}

fn cc_draw_content() {
    let oy = cc_panel_top() as i32;
    unsafe {
        cc_draw_status(oy);
        // ColorOS 控制中心错峰入场：面板展开进度驱动各行卡片按序上移显现
        //（阈值随行号递增，Open/收起全程位置连续；稳态 prog=1 时 k=1、
        // dy=0，零额外视觉差异）。跟手下拉（Drag）同样生效——面板拉到
        // 哪一行哪一行出现，与 ColorOS 手感一致。
        let (k1, dy1) = cc_reveal(0);
        if k1 > 0.0 {
            for i in 0..4usize {
                let (on, icon) = match i {
                    0 => (CC_SW[0], 0u8),
                    1 => (CC_SW[1], 1u8),
                    2 => (CC_SW[2], 2u8),
                    _ => (CC_SW[3], 3u8),
                };
                let scale = cc_el_scale(1, i as u8);
                cc_tile(
                    cc_row1_x(i),
                    CC_ROW1_Y + oy + dy1,
                    CC_BIG,
                    CC_BIG_R,
                    on,
                    icon,
                    scale,
                );
            }
        }
        let (k2, dy2) = cc_reveal(1);
        if k2 > 0.0 {
            for i in 0..4usize {
                let (on, icon) = match i {
                    0 => (CC_FLASHLIGHT, 4u8),
                    1 => (false, 5u8),
                    2 => (false, 6u8),
                    _ => (false, 7u8),
                };
                let scale = cc_el_scale(1, (4 + i) as u8);
                cc_tile(
                    cc_row2_x(i),
                    CC_ROW2_Y + oy + dy2,
                    CC_MED,
                    CC_MED_R,
                    on,
                    icon,
                    scale,
                );
            }
        }
        let (_, dy3) = cc_reveal(2);
        cc_slider(
            CC_MX,
            CC_BR_Y + oy + dy3,
            cc_slider_w(),
            CC_SL_H,
            BRIGHTNESS,
            txt("Brightness", "亮度"),
        );
        let (_, dy4) = cc_reveal(3);
        cc_slider(
            CC_MX,
            CC_VOL_Y + oy + dy4,
            cc_slider_w(),
            CC_SL_H,
            CC_VOLUME,
            txt("Volume", "音量"),
        );
        cc_slider_pop(oy);
        let (_, dy5) = cc_reveal(4);
        cc_music(oy + dy5);
        let (_, dy6) = cc_reveal(5);
        cc_shortcuts(oy + dy6);
    }
}

/// 错峰入场参数：`(显现进度 k∈[0,1], 垂直偏移 dy)`。行 `idx` 在面板进度
/// `[0.30+idx*0.08, 0.55+idx*0.08]` 窗口内从下方 18px 滑入到原位。
fn cc_reveal(idx: u8) -> (f32, i32) {
    unsafe {
        let t0 = 0.30 + idx as f32 * 0.08;
        let k = ((CC_PROG - t0) / 0.25).clamp(0.0, 1.0);
        (k, ((1.0 - k) * 18.0) as i32)
    }
}

fn cc_el_scale(kind: u8, idx: u8) -> f32 {
    unsafe {
        if CC_PRESS_KIND == kind && CC_PRESS_IDX == idx {
            CC_PRESS_SCALE
        } else {
            1.0
        }
    }
}

fn cc_draw_status(oy: i32) {
    let mut hhmm = [0u8; 5];
    time_hhmm(&mut hhmm);
    // 浅色面板：深色文字/图标。
    draw_text(24, 12 + oy, &hhmm, COLOR_TEXT_MAIN);
    let mut date = [0u8; 24];
    let n = cc_date(&mut date);
    draw_text(24, 28 + oy, &date[..n], COLOR_TEXT_DIM);
    draw_battery(478, 16 + oy, COLOR_TEXT_MAIN);
    draw_text(502, 14 + oy, b"85", COLOR_TEXT_MAIN);
}

fn cc_date(buf: &mut [u8]) -> usize {
    let us = now_us();
    let secs = (us / 1_000_000) as i64;
    let days = secs / 86400;
    let (y, mo, d) = civil_from_days(days);
    let mut n = push_dec(buf, y as u32);
    buf[n] = b'-';
    n += 1;
    n += push_dec(&mut buf[n..], mo);
    buf[n] = b'-';
    n += 1;
    n += push_dec(&mut buf[n..], d);
    buf[n] = b' ';
    n += 1;
    let wd = ((days + 4).rem_euclid(7)) as usize;
    let name = if unsafe { LANG_ZH } {
        WD_ZH[wd]
    } else {
        WD_EN[wd]
    };
    let mut i = 0;
    while i < name.len() && n < buf.len() {
        buf[n] = name[i];
        n += 1;
        i += 1;
    }
    n
}

fn cc_tile(x: i32, y: i32, size: i32, r: i32, on: bool, icon: u8, scale: f32) {
    let s = (size as f32 * scale) as i32;
    let ox = (size - s) / 2;
    let oy = (size - s) / 2;
    let tx = x + ox;
    let ty = y + oy;
    let rad = ((r as f32 * scale).max(1.0)) as i32;
    // 浅色开关：开启 = 强调绿底白图标；关闭 = 白色磨砂底深色图标。
    if on {
        fill_round_rect(tx, ty, s, s, rad, COLOR_ACCENT);
    } else {
        cc_fill_round_blend(tx, ty, s, s, rad, 0x00_FF_FF_FF, 205); // 白 80%
    }
    let color = if on { COLOR_TEXT } else { COLOR_TEXT_MAIN };
    cc_icon(icon, tx + s / 2, ty + s / 2, (s / 2).max(8), color);
}

fn cc_fill_round_blend(x: i32, y: i32, w: i32, h: i32, r: i32, color: u32, a: u32) {
    for dy in 0..h {
        for dx in 0..w {
            // 仅四角盒做超采样预判（与 glass_rect_style 同款）：圆角矩形
            // 内部（四个 r×r 角盒之外）的像素免去每像素的浮点
            // in_round_rect 求值（面板滑条/开关 ~80% 像素）。
            let not_corner = !((dx < r && dy < r)
                || (dx >= w - r && dy < r)
                || (dx < r && dy >= h - r)
                || (dx >= w - r && dy >= h - r));
            if not_corner || in_round_rect(dx, dy, w, h, r) {
                let px = x + dx;
                let py = y + dy;
                if px >= 0 && py >= 0 && px < DISPLAY_W as i32 && py < DISPLAY_H as i32 {
                    let bg = unsafe { BACK_BUF[py as usize * DISPLAY_W + px as usize] };
                    put_pixel(px, py, blend(bg, color, a));
                }
            }
        }
    }
}

fn cc_slider(x: i32, y: i32, w: i32, h: i32, value: u32, label: &[u8]) {
    // 轨道：浅灰（深色 10% 叠加在浅色面板上）
    cc_fill_round_blend(x, y, w, h, h / 2, 0x00_1A_1C_22, 26);
    draw_text(x + 14, y + (h - 16) / 2, label, COLOR_TEXT_MAIN);
    let lw = text_width(label);
    let fx = x + 14 + lw + 10;
    let tw = w - (fx - x);
    let fw = (tw as u32 * value / 100).min(tw as u32) as i32;
    if fw > 0 {
        fill_round_rect(fx, y, fw, h, h / 2, COLOR_ACCENT);
    }
    // thumb：20x20 白圆角（强调绿填充上的白色滑块）
    let tx = (fx + fw - 10).clamp(fx, fx + tw - 20);
    let ty = y + (h - 20) / 2;
    fill_round_rect(tx, ty, 20, 20, 10, 0x00_FF_FF_FF);
    // 百分比
    let mut pbuf = [0u8; 4];
    let pn = push_dec(&mut pbuf, value);
    pbuf[pn] = b'%';
    draw_text(x + w + 8, y + (h - 16) / 2, &pbuf[..pn + 1], COLOR_TEXT_MAIN);
}

fn cc_music(oy: i32) {
    unsafe {
        let mut buf = [0u8; 48];
        let mut n = 0usize;
        let prefix = txt("Now Playing: ", "正在播放：");
        let mut i = 0;
        while i < prefix.len() && n < buf.len() {
            buf[n] = prefix[i];
            n += 1;
            i += 1;
        }
        let track = CC_TRACKS[CC_TRACK];
        let mut i = 0;
        while i < track.len() && n < buf.len() {
            buf[n] = track[i];
            n += 1;
            i += 1;
        }
        draw_text(
            (DISPLAY_W as i32 - text_width(&buf[..n])) / 2,
            CC_MUS_Y - 14 + oy,
            &buf[..n],
            COLOR_TEXT_DIM,
        );
        let x0 = cc_mus_x0();
        for i in 0..3usize {
            let bx = x0 + i as i32 * (CC_MUS_W + CC_MUS_GAP);
            let scale = cc_el_scale(2, i as u8);
            let (w, h) = (
                (CC_MUS_W as f32 * scale) as i32,
                (CC_MUS_H as f32 * scale) as i32,
            );
            let ox = (CC_MUS_W - w) / 2;
            let oy2 = (CC_MUS_H - h) / 2;
            cc_fill_round_blend(bx + ox, CC_MUS_Y + oy + oy2, w, h, 18, 0x00_FF_FF_FF, 205);
            let cx = bx + CC_MUS_W / 2;
            let cy = CC_MUS_Y + oy + CC_MUS_H / 2;
            match i {
                0 => cc_music_icon(0, cx, cy),
                1 => {
                    if CC_PLAYING {
                        cc_music_icon(2, cx, cy)
                    } else {
                        cc_music_icon(1, cx, cy)
                    }
                }
                _ => cc_music_icon(3, cx, cy),
            }
        }
    }
}

fn cc_shortcuts(oy: i32) {
    let x0 = cc_short_x0();
    for i in 0..3usize {
        let sx = x0 + i as i32 * (CC_SHORT_W + CC_SHORT_GAP);
        let scale = cc_el_scale(3, i as u8);
        let (w, h) = (
            (CC_SHORT_W as f32 * scale) as i32,
            (CC_SHORT_H as f32 * scale) as i32,
        );
        let ox = (CC_SHORT_W - w) / 2;
        let oy2 = (CC_SHORT_H - h) / 2;
        cc_fill_round_blend(sx + ox, CC_SHORT_Y + oy + oy2, w, h, 24, 0x00_FF_FF_FF, 205);
        let cx = sx + CC_SHORT_W / 2;
        let cy = CC_SHORT_Y + oy + CC_SHORT_H / 2;
        let label = match i {
            0 => txt("Settings", "设置"),
            1 => txt("Lock", "锁屏"),
            _ => txt("Screenshot", "截图"),
        };
        let lw = text_width(label);
        cc_short_icon(i as u8, cx - lw / 2 - 12, cy, COLOR_TEXT_MAIN);
        draw_text(cx - lw / 2 + 14, cy - 8, label, COLOR_TEXT_MAIN);
    }
}

fn cc_mus_x0() -> i32 {
    (DISPLAY_W as i32 - (3 * CC_MUS_W + 2 * CC_MUS_GAP)) / 2
}

fn cc_short_x0() -> i32 {
    (DISPLAY_W as i32 - (3 * CC_SHORT_W + 2 * CC_SHORT_GAP)) / 2
}

// ==================== 图标 ====================

fn cc_icon(kind: u8, cx: i32, cy: i32, s: i32, color: u32) {
    let k = (s / 8).max(1);
    match kind {
        0 => {
            // 飞行模式
            draw_line(cx - 5 * k, cy + 4 * k, cx + 5 * k, cy - 4 * k, color);
            draw_line(cx - 5 * k, cy + 4 * k, cx - 2 * k, cy + 4 * k, color);
            draw_line(cx + 5 * k, cy - 4 * k, cx + 2 * k, cy - 4 * k, color);
            fill_rect(cx - k, cy - k, 2 * k, 2 * k, color);
        }
        1 => draw_wifi(cx - 7, cy - 11, color),
        2 => {
            // 蓝牙
            draw_line(cx, cy - 7, cx, cy + 7, color);
            draw_line(cx, cy - 7, cx + 4, cy - 3, color);
            draw_line(cx + 4, cy - 3, cx, cy, color);
            draw_line(cx, cy, cx + 4, cy + 3, color);
            draw_line(cx + 4, cy + 3, cx, cy + 7, color);
        }
        3 => {
            // 蜂窝数据信号
            for i in 0..4usize {
                let hgt = (3 + i as i32 * 2) * k;
                fill_rect(
                    cx - 5 * k + i as i32 * 3 * k,
                    cy + 6 * k - hgt,
                    (2 * k).max(2),
                    hgt,
                    color,
                );
            }
        }
        4 => {
            // 手电筒
            fill_rect(cx - 3, cy - 7, 6, 10, color);
            fill_rect(cx - 4, cy + 3, 8, 3, color);
            draw_line(cx - 5, cy - 7, cx + 5, cy - 7, color);
        }
        5 => {
            // 计时器
            draw_circle(cx, cy, 5, color);
            fill_rect(cx - 2, cy - 9, 4, 3, color);
            draw_line(cx, cy, cx, cy - 4, color);
            draw_line(cx, cy, cx + 3, cy - 1, color);
        }
        6 => {
            // 计算器
            draw_rect(cx - 6, cy - 8, 12, 15, color);
            fill_rect(cx - 5, cy - 7, 10, 3, color);
            fill_rect(cx - 5, cy - 2, 3, 3, color);
            fill_rect(cx, cy - 2, 3, 3, color);
            fill_rect(cx + 3, cy - 2, 2, 3, color);
            fill_rect(cx - 5, cy + 3, 3, 3, color);
        }
        _ => {
            // 相机
            draw_rect(cx - 7, cy - 5, 14, 10, color);
            fill_rect(cx - 2, cy - 8, 4, 3, color);
            draw_circle(cx, cy, 3, color);
        }
    }
}

fn cc_music_icon(kind: u8, cx: i32, cy: i32) {
    // 浅色面板上的白色磨砂按钮：深色图标。
    let color = COLOR_TEXT_MAIN;
    match kind {
        0 => {
            // 上一首：竖条 + 左三角
            fill_rect(cx - 7, cy - 7, 3, 14, color);
            draw_line(cx + 5, cy - 7, cx - 1, cy, color);
            draw_line(cx - 1, cy, cx + 5, cy + 7, color);
            draw_line(cx + 5, cy + 7, cx + 5, cy - 7, color);
        }
        1 => {
            // 播放：三角
            draw_line(cx - 4, cy - 7, cx + 6, cy, color);
            draw_line(cx + 6, cy, cx - 4, cy + 7, color);
            draw_line(cx - 4, cy + 7, cx - 4, cy - 7, color);
        }
        2 => {
            // 暂停：双竖条
            fill_rect(cx - 6, cy - 7, 4, 14, color);
            fill_rect(cx + 2, cy - 7, 4, 14, color);
        }
        _ => {
            // 下一首：右三角 + 竖条
            draw_line(cx - 5, cy - 7, cx + 1, cy, color);
            draw_line(cx + 1, cy, cx - 5, cy + 7, color);
            draw_line(cx - 5, cy + 7, cx - 5, cy - 7, color);
            fill_rect(cx + 4, cy - 7, 3, 14, color);
        }
    }
}

fn cc_short_icon(kind: u8, cx: i32, cy: i32, color: u32) {
    match kind {
        0 => {
            // 设置：方框 + 中心点
            draw_rect(cx - 5, cy - 5, 10, 10, color);
            fill_rect(cx - 2, cy - 2, 4, 4, 0x00_1E_22_2E);
        }
        1 => {
            // 锁屏：锁形
            fill_rect(cx - 5, cy - 2, 10, 8, color);
            draw_rect(cx - 5, cy - 2, 10, 8, color);
            fill_rect(cx - 3, cy - 8, 6, 6, color);
            draw_rect(cx - 3, cy - 8, 6, 6, color);
        }
        _ => {
            // 截图：圆角框 + 镜头
            draw_rect(cx - 6, cy - 6, 12, 9, color);
            draw_circle(cx, cy - 1, 2, color);
            fill_rect(cx - 1, cy - 8, 2, 2, color);
        }
    }
}

// ==================== 通知中心占位渲染 ====================

fn cc_draw_notif() {
    unsafe {
        if NOTIF_PROG <= 0.001 {
            return;
        }
        let y_top = (NOTIF_H as f32 * (NOTIF_PROG - 1.0)) as i32;
        let y0 = y_top.max(0);
        let y1 = (y_top + NOTIF_H).min(DISPLAY_H as i32);
        for y in y0..y1 {
            let r = y - y_top;
            let row = y as usize * DISPLAY_W;
            for x in 0..DISPLAY_W as i32 {
                if r >= NOTIF_H - 20 {
                    let cx2 = if x < 20 {
                        20 - x
                    } else if x >= DISPLAY_W as i32 - 20 {
                        x - (DISPLAY_W as i32 - 1 - 20)
                    } else {
                        0
                    };
                    let cy2 = r - (NOTIF_H - 1 - 20);
                    if cx2 * cx2 + cy2 * cy2 > 20 * 20 {
                        continue;
                    }
                }
                BACK_BUF[row + x as usize] = blend(BACK_BUF[row + x as usize], 0x00_FF_FF_FF, 232);
            }
        }
        // 浅色通知面板：深色标题。
        let title = txt("NOTIFICATIONS", "通知中心");
        draw_text(24, y_top + 20, title, COLOR_TEXT_MAIN);
        let samples: [(&[u8], &[u8]); 3] = [
            (
                txt("KunxOS", "KunxOS"),
                txt("Welcome to KunxOS", "欢迎使用 KunxOS"),
            ),
            (
                txt("System", "系统"),
                txt("Wi-Fi connected", "无线网络已连接"),
            ),
            (
                txt("Music", "音乐"),
                txt("Neon Nights is playing", "正在播放：Neon Nights"),
            ),
        ];
        for i in 0..3usize {
            let ny = y_top + 64 + i as i32 * 76;
            // 通知卡片：浅色面板上的浅灰卡（深色 9% 叠加）。
            cc_fill_round_blend(16, ny, DISPLAY_W as i32 - 32, 64, 16, 0x00_1A_1C_22, 24);
            draw_text(36, ny + 12, samples[i].0, COLOR_TEXT_DIM);
            draw_text(36, ny + 34, samples[i].1, COLOR_TEXT_MAIN);
        }
    }
}
