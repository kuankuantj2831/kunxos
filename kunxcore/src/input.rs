//! 输入子系统：Linux 兼容的 `struct input_event` 事件流与 `/dev/input/event0`。
//!
//! - 事件设备：环形缓冲 + 阻塞/非阻塞读取，注册到 devfs（主设备号 13）。
//! - 事件生产：解析主 UART 上的触摸协议帧（QEMU 简化方案），产生触摸事件。
//!   协议帧（6 字节，小端）：
//!     `0xAA 0x01 x_lo x_hi y_lo y_hi`  触摸按下（坐标 x,y）
//!     `0xAA 0x02 x_lo x_hi y_lo y_hi`  触摸移动（坐标 x,y）
//!     `0xAA 0x00 0x00 0x00 0x00 0x00`  触摸抬起
//! - 多点触摸：单点跑通，多点（`ABS_MT_*`）为骨架。

use core::arch::asm;

use crate::compositor;
use crate::devfs;
use crate::gesture;
use crate::sched_cfs;
use crate::timer;
use crate::vfs;

// ==================== input_event（Linux 兼容） ====================

/// 事件类型。
pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const EV_ABS: u16 = 0x03;

/// EV_SYN 同步码。
pub const SYN_REPORT: u16 = 0x00;

/// EV_ABS 绝对轴码。
pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const ABS_MT_SLOT: u16 = 0x2f;
pub const ABS_MT_TRACKING_ID: u16 = 0x39;
pub const ABS_MT_POSITION_X: u16 = 0x35;
pub const ABS_MT_POSITION_Y: u16 = 0x36;

/// EV_KEY 按键码。
pub const BTN_TOUCH: u16 = 0x14a; // 330
pub const BTN_LEFT: u16 = 0x110; // 272

/// Linux `struct input_event`（24 字节，小端）：
/// `timeval`（tv_sec:i64 + tv_usec:i64）+ type:u16 + code:u16 + value:i32。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct InputEvent {
    pub tv_sec: i64,
    pub tv_usec: i64,
    pub etype: u16,
    pub code: u16,
    pub value: i32,
}

impl InputEvent {
    /// 构造一个时间戳为零的事件（由调用方按需填充时间）。
    pub const fn new(etype: u16, code: u16, value: i32) -> Self {
        InputEvent { tv_sec: 0, tv_usec: 0, etype, code, value }
    }

    /// 序列化为 Linux 布局的 24 字节（小端）。
    pub fn to_bytes(&self) -> [u8; 24] {
        let mut b = [0u8; 24];
        b[0..8].copy_from_slice(&self.tv_sec.to_le_bytes());
        b[8..16].copy_from_slice(&self.tv_usec.to_le_bytes());
        b[16..18].copy_from_slice(&self.etype.to_le_bytes());
        b[18..20].copy_from_slice(&self.code.to_le_bytes());
        b[20..24].copy_from_slice(&self.value.to_le_bytes());
        b
    }
}

// ==================== 环形缓冲 ====================

/// 事件环形缓冲容量（完整事件数）。
const EV_BUF_SIZE: usize = 64;

/// 每个设备可同时登记的阻塞读等待者数（多任务并发阻塞读不互相覆盖）。
const WAITERS: usize = 4;

/// 屏蔽 IRQ 并返回原 DAIF 值（供 `irq_restore` 恢复，避免无条件开中断
/// 破坏嵌套屏蔽上下文 —— 若调用方已处于屏蔽态，`daifclr` 会提前开中断）。
unsafe fn irq_save() -> u64 {
    let d: u64;
    asm!("mrs {}, daif", out(reg) d);
    asm!("msr daifset, #2");
    d
}

/// 恢复调用前的 DAIF 状态。
unsafe fn irq_restore(saved: u64) {
    asm!("msr daif, {}", in(reg) saved);
}

/// 输入设备：事件环形缓冲 + 阻塞读等待者。
struct InputDevice {
    name: [u8; 32],
    /// 设备名长度（不含结尾 NUL）。
    name_len: usize,
    ev_buf: [InputEvent; EV_BUF_SIZE],
    head: usize,
    tail: usize,
    count: usize,
    /// 阻塞读等待者（多槽，多任务并发读不丢唤醒）。
    waiter: [Option<u32>; WAITERS],
    /// 最近一次非阻塞轮询的任务，用于事件到达时打断其帧等待。
    poller: Option<u32>,
    /// 最近一次读取本设备的任务（前台 UI 应用）：指针/按键事件到达时对其
    /// 做调度加速（boost_tid），交互任务即时抢占后台负载。
    reader_tid: u32,
    /// 上次触控加速时刻（µs），限频用。
    last_boost_us: u64,
    /// 触摸面板 X 轴物理最大值。
    max_x: u32,
    /// 触摸面板 Y 轴物理最大值。
    max_y: u32,
}

const EMPTY_EVENT: InputEvent =
    InputEvent { tv_sec: 0, tv_usec: 0, etype: 0, code: 0, value: 0 };

/// 最近一次指针/触摸事件到达时刻（µs，单调时钟）。
///
/// 触控→上屏延迟（t2f）度量的采样起点：呈现路径用 `now − 该值` 估计输入
/// 跟随延迟（perf.rs 的 t2f 统计）。事件本身已带时间戳（evdev timeval），
/// 此处冗余记录最近一拍，呈现侧无需解析事件缓冲。
static mut LAST_PTR_US: u64 = 0;

/// 最近一次指针/触摸事件到达时刻（µs；尚无事件为 0）。t2f 度量数据源。
pub fn last_input_us() -> u64 {
    unsafe { LAST_PTR_US }
}

static mut DEV: InputDevice = InputDevice {
    name: [0; 32],
    name_len: 0,
    ev_buf: [EMPTY_EVENT; EV_BUF_SIZE],
    head: 0,
    tail: 0,
    count: 0,
    waiter: [None; WAITERS],
    poller: None,
    reader_tid: 0,
    last_boost_us: 0,
    max_x: 0,
    max_y: 0,
};

/// 设备名（EVIOCGNAME 返回）。
const DEV_NAME: &str = "KunxOS Touch + Keyboard";

// ==================== ioctl ====================

/// `EVIOCGNAME(len=256)` = `_IOR('E', 0x06, 256)`。
const EVIOCGNAME: u32 = 0x8100_4506;
/// `EVIOCGBIT(0, 32)` = `_IOR('E', 0x20, 32)`：查询支持的事件类型位图。
const EVIOCGBIT_EV: u32 = 0x8020_4520;
/// 支持的事件类型位图：EV_SYN | EV_KEY | EV_ABS。
const SUPPORTED_EV: u32 = (1u32 << EV_SYN) | (1u32 << EV_KEY) | (1u32 << EV_ABS);

// ==================== UART 协议帧解析 ====================

/// 协议帧起始字节。
const FRAME_SYNC: u8 = 0xAA;
const FRAME_LEN: usize = 6;
/// 帧类型。
const FRAME_UP: u8 = 0x00;
const FRAME_DOWN: u8 = 0x01;
const FRAME_MOVE: u8 = 0x02;

/// UART 协议帧解析状态机（静态，位于 .bss）。
struct Parser {
    buf: [u8; FRAME_LEN],
    n: usize,
}

const EMPTY_PARSER: Parser = Parser { buf: [0; FRAME_LEN], n: 0 };

static mut PARSER: Parser = EMPTY_PARSER;

/// 向 UART 协议解析器喂入一个字节。
///
/// 返回 `true` 表示该字节被触摸协议帧消费（不应再送入 tty/shell 缓冲）；
/// 返回 `false` 表示是普通串口字节（交给 shell 行编辑等）。
pub fn feed_uart_byte(c: u8) -> bool {
    unsafe {
        if PARSER.n == 0 {
            if c == FRAME_SYNC {
                PARSER.buf[0] = c;
                PARSER.n = 1;
                return true;
            }
            return false;
        }
        // 帧进行中：第二字节必须是合法帧类型，否则视为伪 0xAA / 数据损坏，
        // 复位解析器并把当前字节交给 shell，避免吞掉后续用户输入并产生垃圾事件。
        if PARSER.n == 1 && !matches!(c, FRAME_UP | FRAME_DOWN | FRAME_MOVE) {
            PARSER.n = 0;
            return false;
        }
        PARSER.buf[PARSER.n] = c;
        PARSER.n += 1;
        if PARSER.n == FRAME_LEN {
            PARSER.n = 0;
            let frame = PARSER.buf;
            handle_frame(&frame);
        }
        true
    }
}

/// 处理一个完整的 6 字节协议帧，生成对应的 `input_event` 序列。
fn handle_frame(frame: &[u8; FRAME_LEN]) {
    let kind = frame[1];
    let mut x = u16::from_le_bytes([frame[2], frame[3]]) as i32;
    let mut y = u16::from_le_bytes([frame[4], frame[5]]) as i32;
    // 钳制到设备物理最大值（max_x/max_y 未初始化时保持原值），
    // 防止损坏/伪造帧产生越界坐标（消费端按设备范围解释）
    unsafe {
        if DEV.max_x > 0 {
            x = x.min(DEV.max_x as i32);
        }
        if DEV.max_y > 0 {
            y = y.min(DEV.max_y as i32);
        }
    }

    let us = timer::now_us();
    let sec = (us / 1_000_000) as i64;
    let usec = (us % 1_000_000) as i64;

    // 转发给全屏手势系统（设备坐标 → 屏幕坐标）
    let gk = match kind {
        FRAME_DOWN => Some(gesture::InputKind::Down),
        FRAME_MOVE => Some(gesture::InputKind::Move),
        FRAME_UP => Some(gesture::InputKind::Up),
        _ => None,
    };
    if let Some(gk) = gk {
        let sw = compositor::width() as i64;
        let sh = compositor::height() as i64;
        let (mx, my) = unsafe { (DEV.max_x as i64, DEV.max_y as i64) };
        let sx = if mx > 0 { (x as i64 * sw / mx) as i32 } else { x };
        let sy = if my > 0 { (y as i64 * sh / my) as i32 } else { y };
        gesture::handle(gesture::PointerEvent { kind: gk, x: sx, y: sy });
    }

    match kind {
        FRAME_DOWN => {
            emit(sec, usec, EV_ABS, ABS_X, x);
            emit(sec, usec, EV_ABS, ABS_Y, y);
            emit(sec, usec, EV_KEY, BTN_TOUCH, 1);
            emit(sec, usec, EV_SYN, SYN_REPORT, 0);
        }
        FRAME_MOVE => {
            emit(sec, usec, EV_ABS, ABS_X, x);
            emit(sec, usec, EV_ABS, ABS_Y, y);
            emit(sec, usec, EV_SYN, SYN_REPORT, 0);
        }
        FRAME_UP => {
            emit(sec, usec, EV_KEY, BTN_TOUCH, 0);
            emit(sec, usec, EV_SYN, SYN_REPORT, 0);
        }
        _ => {} // 未知帧类型：忽略
    }
}

/// 生成一个带时间戳的事件并写入环形缓冲。
fn emit(sec: i64, usec: i64, etype: u16, code: u16, value: i32) {
    push_event(InputEvent { tv_sec: sec, tv_usec: usec, etype, code, value });
}

// ==================== 事件缓冲操作 ====================

/// 向环形缓冲写入一个事件；缓冲满时丢弃最旧事件（保留最新状态）。
///
/// 本函数在设备中断处理例程中被调用（IRQ 已屏蔽），唤醒等待者时使用
/// [`sched_cfs::wake_irq`] 避免中途重新使能 IRQ。
pub fn push_event(ev: InputEvent) {
    unsafe {
        // 指针移动事件合并（Android 输入批处理思路）：积压过半时丢弃最旧
        // 的完整移动帧（ABS_X,ABS_Y,SYN 三元组）。应用繁忙（整帧重绘）时
        // 新位置直接覆盖旧位置，光标恢复后不再「追赶式」逐帧爬行。
        if ev.etype == EV_ABS && DEV.count >= EV_BUF_SIZE / 2 {
            while DEV.count >= EV_BUF_SIZE / 2 {
                let h = DEV.head;
                let e0 = DEV.ev_buf[h];
                let e1 = DEV.ev_buf[(h + 1) % EV_BUF_SIZE];
                let e2 = DEV.ev_buf[(h + 2) % EV_BUF_SIZE];
                if e0.etype == EV_ABS
                    && e0.code == ABS_X
                    && e1.etype == EV_ABS
                    && e1.code == ABS_Y
                    && e2.etype == EV_SYN
                {
                    DEV.head = (h + 3) % EV_BUF_SIZE;
                    DEV.count -= 3;
                } else {
                    break;
                }
            }
        }
        if DEV.count == EV_BUF_SIZE {
            // 缓冲全满：丢弃最旧的一个**完整帧**（到第一个 SYN 含）而不是
            // 单个事件 —— 单事件丢弃可能把 DOWN 帧劈开（BTN_TOUCH=1 与它
            // 的 ABS_X/ABS_Y 分离），用户态在 BTN_TOUCH 时记录的按下点退化
            // 为悬停旧位置。最多丢一帧触点（积压场景可恢复），不破坏帧界。
            let mut k = 0usize;
            while k < DEV.count {
                let e = DEV.ev_buf[(DEV.head + k) % EV_BUF_SIZE];
                k += 1;
                if e.etype == EV_SYN {
                    break;
                }
            }
            DEV.head = (DEV.head + k) % EV_BUF_SIZE;
            DEV.count -= k;
        }
        DEV.ev_buf[DEV.tail] = ev;
        DEV.tail = (DEV.tail + 1) % EV_BUF_SIZE;
        DEV.count += 1;
        // 唤醒全部阻塞读者（多读者并发阻塞不丢唤醒）
        for w in DEV.waiter.iter_mut() {
            if let Some(t) = w.take() {
                sched_cfs::wake_irq(t);
            }
        }
        // launcher 的帧循环可能正阻塞于 nanosleep：提前唤醒最近轮询
        // event0 的任务，不打断其它后台服务的正常睡眠。poller 登记**不
        // 消费**：此前 take() 会把「登记 poller 后、注册睡眠前」到达的
        // 事件竞态变成白睡一个帧间隔（输入延迟 +16ms）。未睡眠时
        // wake_sleeper_irq 无副作用，登记由下次 input_read 覆盖。
        if let Some(p) = DEV.poller {
            sched_cfs::wake_sleeper_irq(p);
        }
        // 输入触控加速（鸿蒙交互调度 / ColorOS 潮汐引擎思路）：指针/按键
        // 事件到达时，把最近读取输入的 UI 任务 vruntime 拉到全局最小并
        // 请求唤醒抢占 —— 交互任务即时抢占后台负载，输入→画面延迟不再
        // 包含「等后台任务用完 10ms 时间片」。限频 2ms。
        if ev.etype == EV_ABS
            || ev.etype == EV_REL
            || (ev.etype == EV_KEY && (ev.code == BTN_TOUCH || ev.code == BTN_LEFT))
        {
            let now = timer::now_us();
            LAST_PTR_US = now;
            if DEV.reader_tid != 0 && now.saturating_sub(DEV.last_boost_us) >= 2_000 {
                DEV.last_boost_us = now;
                sched_cfs::boost_tid(DEV.reader_tid);
                // 一次性调度 deadline：最近读者须在本帧内（60Hz 预算 16.7ms）
                // 被调度并消费期限——期限语义保证即使 boost 后中途阻塞、
                // 再唤醒也保持下一调度决策的优先权。
                sched_cfs::set_tid_deadline(DEV.reader_tid, now + 16_667);
            }
        }
    }
}

/// 从环形缓冲取出事件并序列化为字节流，返回写入的字节数（完整事件数 × 24）。
///
/// 与 IRQ 上下文的 `push_event` 并发，必须屏蔽 IRQ 保证 head/count 不被撕裂。
fn read_events(buf: &mut [u8]) -> i32 {
    let saved = unsafe { irq_save() };
    let max_ev = buf.len() / 24;
    let n = max_ev.min(unsafe { DEV.count });
    let mut off = 0usize;
    for _ in 0..n {
        let ev = unsafe { DEV.ev_buf[DEV.head] };
        unsafe {
            DEV.head = (DEV.head + 1) % EV_BUF_SIZE;
            DEV.count -= 1;
        }
        let bytes = ev.to_bytes();
        buf[off..off + 24].copy_from_slice(&bytes);
        off += 24;
    }
    unsafe { irq_restore(saved) };
    off as i32
}

// ==================== devfs 接口 ====================

/// `/dev/input/event0` 的 read 实现。
///
/// 返回已读取的字节数（完整 `input_event` 的整数倍）；非阻塞且无事件时返回
/// `-EAGAIN`（-11）；阻塞读在无事件时挂起当前任务，直到有事件到达被唤醒。
fn input_read(_minor: u32, flags: u32, _offset: usize, buf: &mut [u8]) -> i32 {
    if buf.len() < 24 {
        return 0;
    }
    loop {
        let n = read_events(buf);
        if n > 0 {
            return n;
        }
        if flags & vfs::O_NONBLOCK != 0 {
            // 非阻塞路径登记 poller 也须屏蔽 IRQ（与 IRQ 上下文 push_event 并发）
            let saved = unsafe { irq_save() };
            unsafe {
                DEV.poller = Some(sched_cfs::current_tid());
                // 登记最近读者：指针事件到达时对其做调度加速（触控加速）
                DEV.reader_tid = sched_cfs::current_tid();
            }
            unsafe { irq_restore(saved) };
            return -11; // EAGAIN
        }
        // 阻塞：屏蔽 IRQ 后二次检查，避免「登记等待者 → 挂起」之间丢失中断唤醒。
        let saved = unsafe { irq_save() };
        let ready = unsafe { DEV.count > 0 };
        if ready {
            unsafe { irq_restore(saved) };
            continue;
        }
        unsafe {
            // 登记等待者（多槽）；无空位则让出重试，避免永久丢唤醒
            let mut registered = false;
            for w in DEV.waiter.iter_mut() {
                if w.is_none() {
                    *w = Some(sched_cfs::current_tid());
                    registered = true;
                    break;
                }
            }
            // 阻塞读者同样是最近读者（触控加速目标）
            DEV.reader_tid = sched_cfs::current_tid();
            if !registered {
                irq_restore(saved);
                sched_cfs::yield_now();
                continue;
            }
        }
        // block_current 切换任务并在返回时恢复 IRQ。
        sched_cfs::block_current();
        unsafe { irq_restore(saved) };
    }
}

/// `/dev/input/event0` 的 write：evdev 不支持写，返回 -1。
fn input_write(_minor: u32, _buf: &[u8]) -> i32 {
    -1
}

/// `/dev/input/event0` 的 ioctl 实现：把查询结果写入 `out`，返回字节数；
/// 未识别的请求返回 `-ENOTTY`（-25）。
fn input_ioctl(_minor: u32, request: u32, out: &mut [u8]) -> i32 {
    match request {
        EVIOCGNAME => {
            let name = unsafe { &DEV.name[..DEV.name_len] };
            let n = name.len().min(out.len());
            out[..n].copy_from_slice(&name[..n]);
            n as i32
        }
        EVIOCGBIT_EV => {
            if out.len() < 4 {
                return 0;
            }
            out[..4].copy_from_slice(&SUPPORTED_EV.to_le_bytes());
            4
        }
        _ => -25,
    }
}

// ==================== 初始化 ====================

/// 初始化输入子系统：注册事件设备并创建 `/dev/input/event0`。
///
/// `info` 由 HAL 平台后端提供；`None` 表示平台无输入设备（如 Pi 4 骨架），
/// 此时仍注册设备节点（launcher 打开失败会回退为模拟输入）。
pub fn init(info: Option<kunxhal::input::InputInfo>) {
    unsafe {
        if let Some(i) = info {
            DEV.max_x = i.max_x;
            DEV.max_y = i.max_y;
        }
        let n = DEV_NAME.len().min(DEV.name.len());
        DEV.name[..n].copy_from_slice(&DEV_NAME.as_bytes()[..n]);
        DEV.name_len = n;
    }

    devfs::register_chrdev(
        devfs::INPUT_MAJOR,
        "input",
        devfs::ChrOps { read: input_read, write: input_write, ioctl: input_ioctl },
    );

    // 创建 /dev/input 目录与 event0 设备节点（重复创建时 mknod 失败，可忽略）。
    vfs::mkdir("/dev/input");
    vfs::mknod("/dev/input/event0", vfs::FileType::Chr, devfs::INPUT_MAJOR, devfs::INPUT_MINOR);

    crate::kinfo! {
        crate::uart_send_str("[input] /dev/input/event0 已注册 (");
        crate::uart_send_str(DEV_NAME);
        crate::uart_send_str(")\n");
    }
}
