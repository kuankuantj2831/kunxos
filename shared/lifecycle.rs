//! KunxOS 应用生命周期客户端（EL0 用户态，经 `app_kit` 引入）。
//!
//! 与 lifd（生命周期守护进程）通过 `/data/life` 下的文件信箱通信：
//!
//! ```text
//! /data/life/
//! ├── reg_<name>      launcher 启动应用时登记（内容 "pid FG"），lifd 读取后删除
//! ├── cmd_<pid>       应用/launcher → lifd 的命令（BG / RECENTS / BTASK ...）
//! ├── evt_<pid>       lifd → 应用的事件（PAUSE / SUSPEND / RESUME / TERMINATE /
//! │                   NOTIFY / BG_TASK_EXPIRED ...）；"最新覆盖"语义，应用读后清空
//! └── launcher_evt    lifd → launcher 的事件（BG / FG / KILLED / EXITED）
//! ```
//!
//! 内核侧配合：SIGSTOP 停止进程（后台）、SIGCONT 恢复（回到前台）——均在内核
//! `signal.rs`/`sched_cfs.rs` 实现，本模块只负责事件信箱与回调分发。
//!
//! 应用接入方式（app_kit::poll_input 已自动调用 `lc_poll`）：
//! 1. 替换 `exit(nav_exit_code(ev.nav))` 为 `lc_handle_nav(ev.nav)`：
//!    Back=退出；Home=后台化（阻塞至恢复）；Recents=后台化+最近任务。
//! 2. 需要状态保存的应用：`lc_set_callbacks(...)` 注册
//!    on_pause/on_resume/on_suspend/on_terminate/on_notify。

// 事件/命令协议常量（与 lifd 保持一致）。
pub const LC_DIR: &str = "/data/life";
pub const LC_EVT_PREFIX: &str = "/data/life/evt_";
pub const LC_CMD_PREFIX: &str = "/data/life/cmd_";
pub const LC_STATE_DIR: &str = "/data/state";
pub const LC_PUSH_IN_DIR: &str = "/data/push/in";

/// 后台任务类型（与 lifd 白名单一致）。
pub const BGT_AUDIO: u8 = 0;
pub const BGT_LOCATION: u8 = 1;
pub const BGT_VOIP: u8 = 2;
pub const BGT_DOWNLOAD: u8 = 3;

// 事件类型码（evt 文件内容首字段）。
const EV_PAUSE: u8 = 1;
const EV_SUSPEND: u8 = 2;
const EV_RESUME: u8 = 3;
const EV_TERMINATE: u8 = 4;
const EV_BG_EXPIRED: u8 = 5;
const EV_NOTIFY: u8 = 6;

/// 生命周期事件（lc_poll 返回值）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LcEvent {
    pub kind: u8, // 0 = 无事件；其余见 EV_* 常量
    pub id: u32,  // 通知 id（NOTIFY 事件）
    pub title: [u8; 48],
    pub body: [u8; 96],
}

impl LcEvent {
    pub const fn none() -> LcEvent {
        LcEvent { kind: 0, id: 0, title: [0; 48], body: [0; 96] }
    }
    pub fn is_none(&self) -> bool {
        self.kind == 0
    }
}

/// 生命周期回调（应用注册；默认全 no-op，on_resume 默认触发重绘标记）。
#[derive(Clone, Copy)]
pub struct LcCallbacks {
    /// 进入后台：暂停动画/音频、轻量清理。
    pub on_pause: fn(),
    /// 回到前台：重绘/恢复（默认设置 lc_needs_redraw 标记）。
    pub on_resume: fn(),
    /// 挂起：保存 UI 状态到 SFS（`lc_save_state`）。
    pub on_suspend: fn(),
    /// 终止前：最终保存。
    pub on_terminate: fn(),
    /// 本地推送到达。
    pub on_notify: fn(&LcEvent),
    /// 后台任务超时被回收。
    pub on_bg_expired: fn(),
}

const fn cb_nop() {}
fn cb_resume_default() {
    // 默认 on_resume：设置重绘标记（应用可经 lc_needs_redraw 查询）。
    // 视觉恢复由 launcher 的帧快照保证，此标记供需要主动重绘的应用使用。
    unsafe { LC_RESUMED = true; }
}

/// 应用名（从 argv[0] 基名推导；状态文件用）。
static mut LC_NAME: [u8; 32] = [0; 32];
static mut LC_NAME_LEN: usize = 0;
static mut LC_INITED: bool = false;
static mut LC_PID: u32 = 0;
/// 恢复标记：on_resume 后置位，应用查询后应清除。
static mut LC_RESUMED: bool = false;
/// 本次 lc_poll 的事件缓冲（避免栈上大数组反复构造）。
static mut LC_EVT_BUF: [u8; 160] = [0; 160];
static mut LC_CALLBACKS: LcCallbacks = LcCallbacks {
    on_pause: cb_nop,
    on_resume: cb_resume_default,
    on_suspend: cb_nop,
    on_terminate: cb_nop,
    on_notify: |_| {},
    on_bg_expired: cb_nop,
};

/// 注册生命周期回调（覆盖默认）。
pub fn lc_set_callbacks(cb: LcCallbacks) {
    unsafe {
        LC_CALLBACKS = cb;
    }
}

/// 应用自报名称（用于状态文件 /data/state/<name>.bin）。不调用则从 argv[0] 推导。
pub fn lc_set_name(name: &[u8]) {
    unsafe {
        let n = name.len().min(31);
        LC_NAME[..n].copy_from_slice(&name[..n]);
        LC_NAME[n] = 0;
        LC_NAME_LEN = n;
    }
}

/// 懒初始化：pid、应用名、事件信箱存在性探测。失败返回 false
/// （lifd 未就绪；调用方应回退旧行为）。
pub fn lc_init() -> bool {
    unsafe {
        if LC_INITED {
            return true;
        }
        LC_INITED = true;
        // pid
        LC_PID = super::syscall1(super::SYS_GETPID, 0) as u32;
        // 应用名：默认 "app"；需状态保存的应用调用 lc_set_name 覆盖。
        if LC_NAME_LEN == 0 {
            let s = b"app";
            LC_NAME[..s.len()].copy_from_slice(s);
            LC_NAME_LEN = s.len();
        }
        // 事件文件存在性探测：目录不存在视为 lifd 未启动
        let mut path = [0u8; 64];
        let n = mk_evt_path(&mut path);
        let fd = super::open(core::str::from_utf8(&path[..n]).unwrap_or(""), super::O_RDONLY);
        if fd < 0 {
            // 不销毁 INITED：后续 lc_poll 仍会尝试（lifd 可能稍后启动）
            return false;
        }
        super::syscall1(super::SYS_CLOSE, fd as usize);
        true
    }
}

fn mk_evt_path(out: &mut [u8; 64]) -> usize {
    unsafe {
        let p = LC_EVT_PREFIX.as_bytes();
        out[..p.len()].copy_from_slice(p);
        let n = p.len();
        let mut tmp = [0u8; 10];
        let mut i = tmp.len();
        let mut v = LC_PID as u64;
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
        let d = tmp.len() - i;
        out[n..n + d].copy_from_slice(&tmp[i..]);
        n + d
    }
}

fn mk_cmd_path(out: &mut [u8; 64]) -> usize {
    unsafe {
        let p = LC_CMD_PREFIX.as_bytes();
        out[..p.len()].copy_from_slice(p);
        let n = p.len();
        let mut tmp = [0u8; 10];
        let mut i = tmp.len();
        let mut v = LC_PID as u64;
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
        let d = tmp.len() - i;
        out[n..n + d].copy_from_slice(&tmp[i..]);
        n + d
    }
}

/// 写一条命令到命令信箱（覆盖式）。
fn lc_write_cmd(cmd: &[u8]) -> bool {
    unsafe {
        let mut path = [0u8; 64];
        let n = mk_cmd_path(&mut path);
        let fd = super::open(
            core::str::from_utf8(&path[..n]).unwrap_or(""),
            super::O_WRONLY | 0x40 | 0x200, // O_CREAT | O_TRUNC
        );
        if fd < 0 {
            return false;
        }
        let r = super::syscall3(super::SYS_WRITE, fd as usize, cmd.as_ptr() as usize, cmd.len());
        super::syscall1(super::SYS_CLOSE, fd as usize);
        (r as isize) == cmd.len() as isize
    }
}

/// 读取事件文件并清空；无事件返回空。返回事件内容（不含尾部 NUL）。
fn lc_read_evt(buf: &mut [u8]) -> usize {
    unsafe {
        let mut path = [0u8; 64];
        let n = mk_evt_path(&mut path);
        let fd = super::open(core::str::from_utf8(&path[..n]).unwrap_or(""), super::O_RDONLY);
        if fd < 0 {
            return 0;
        }
        let r = super::syscall3(super::SYS_READ, fd as usize, buf.as_mut_ptr() as usize, buf.len())
            as isize;
        super::syscall1(super::SYS_CLOSE, fd as usize);
        if r <= 0 {
            return 0;
        }
        let len = (r as usize).min(buf.len());
        // 读后清空（unlink + 重建为空文件：保证下一次 lc_poll 读不到旧事件）
        super::syscall3(super::SYS_UNLINKAT, super::AT_FDCWD, path.as_ptr() as usize, 0);
        // 重建空文件（lifd 用 O_CREAT 追加语义写事件；先建好避免 lifd 写入失败）
        let fd2 = super::open(
            core::str::from_utf8(&path[..n]).unwrap_or(""),
            super::O_WRONLY | 0x40 | 0x200,
        );
        if fd2 >= 0 {
            super::syscall1(super::SYS_CLOSE, fd2 as usize);
        }
        len
    }
}

/// 解析事件内容并执行回调；返回事件。
pub fn lc_poll() -> LcEvent {
    unsafe {
        if !LC_INITED {
            lc_init();
        }
        let n = lc_read_evt(&mut LC_EVT_BUF);
        if n == 0 {
            return LcEvent::none();
        }
        let s = &LC_EVT_BUF[..n];
        let mut ev = LcEvent::none();
        // 事件格式：EVT <id> <payload>
        //   PAUSE / SUSPEND / RESUME / TERMINATE / BG_TASK_EXPIRED 无 payload
        //   NOTIFY <id> <title>|<body>
        //
        // lifd 后台化时写合并事件 "PAUSE SUSPEND"：应用应在 SIGSTOP 前的
        // 宽限窗内先执行 on_pause（停动画/音频）再执行 on_suspend（保存
        // UI 状态到 SFS）。这里把两个关键字都检出，回调按顺序各触发一次。
        // lifd 恢复时也可能写合并事件 "RESUME [NOTIFY ...]"（挂起期间延迟
        // 补投的通知追加在 RESUME 之后）——应同时触发 on_resume 与 on_notify。
        let evt_pause_dual = s.starts_with(b"PAUSE") && s.windows(7).any(|w| w == b"SUSPEND");
        let mut evt_resume_notify: Option<LcEvent> = None;
        if s.starts_with(b"PAUSE") {
            ev.kind = EV_PAUSE;
        } else if s.starts_with(b"SUSPEND") {
            ev.kind = EV_SUSPEND;
        } else if s.starts_with(b"RESUME") {
            ev.kind = EV_RESUME;
            // "RESUME NOTIFY <id> <title>|<body>"：恢复时补投的延迟通知
            if let Some(pos) = (0..s.len().saturating_sub(6)).find(|&i| &s[i..i + 6] == b"NOTIFY")
            {
                let mut ne = LcEvent::none();
                ne.kind = EV_NOTIFY;
                parse_notify(&s[pos..], &mut ne);
                evt_resume_notify = Some(ne);
            }
        } else if s.starts_with(b"TERMINATE") {
            ev.kind = EV_TERMINATE;
        } else if s.starts_with(b"BG_TASK_EXPIRED") {
            ev.kind = EV_BG_EXPIRED;
        } else if s.starts_with(b"NOTIFY") {
            ev.kind = EV_NOTIFY;
            parse_notify(s, &mut ev);
        } else {
            return ev; // 未知事件：忽略
        }
        // 分发回调 + 默认动作
        let cb = LC_CALLBACKS;
        match ev.kind {
            EV_PAUSE => {
                (cb.on_pause)();
                // 合并事件 "PAUSE SUSPEND"：宽限窗内紧接着执行状态保存
                //（lifd 在发送后等待 BG_GRACE_US 才 SIGSTOP，两回调都完成）。
                if evt_pause_dual {
                    (cb.on_suspend)();
                }
            }
            EV_SUSPEND => (cb.on_suspend)(),
            EV_RESUME => {
                (cb.on_resume)();
                // 恢复时补投的延迟通知：on_resume 之后按通知回调触发
                if let Some(ne) = evt_resume_notify {
                    (cb.on_notify)(&ne);
                }
                // 应用可用 lc_needs_redraw() 查询（默认回调已置位）
            }
            EV_TERMINATE => {
                (cb.on_terminate)();
                super::exit(0); // 保存后优雅退出；lifd 的 SIGKILL 兜底
            }
            EV_BG_EXPIRED => (cb.on_bg_expired)(),
            EV_NOTIFY => (cb.on_notify)(&ev),
            _ => {}
        }
        ev
    }
}

/// 解析 NOTIFY 内容（"NOTIFY <id> <title>|<body>" 或去掉前缀的
/// "<id> <title>|<body>"）到事件结构。
fn parse_notify(s: &[u8], ev: &mut LcEvent) {
    let mut it = s.split(|&c| c == b' ' || c == b'|');
    let _ = it.next(); // "NOTIFY"（无前缀时即 "<id>…" 的首段）
    if let Some(id) = it.next() {
        let mut v: u32 = 0;
        for &c in id {
            if c >= b'0' && c <= b'9' {
                v = v.wrapping_mul(10).wrapping_add((c - b'0') as u32);
            }
        }
        ev.id = v;
    }
    if let Some(t) = it.next() {
        let k = t.len().min(ev.title.len() - 1);
        ev.title[..k].copy_from_slice(&t[..k]);
    }
    if let Some(b) = it.next() {
        let k = b.len().min(ev.body.len() - 1);
        ev.body[..k].copy_from_slice(&b[..k]);
    }
}

/// 恢复后是否需要重绘（on_resume 置位；应用处理后调用 lc_clear_redraw 清除）。
pub fn lc_needs_redraw() -> bool {
    unsafe { LC_RESUMED }
}

pub fn lc_clear_redraw() {
    unsafe { LC_RESUMED = false; }
}

/// Home 手势：请求后台化并阻塞，直到 lifd 恢复本应用（SIGCONT + RESUME）。
///
/// 流程：写 BG 命令 → lifd 收到后发送 PAUSE/SUSPEND 事件并 SIGSTOP 本进程
/// （内核停止调度）→ 用户再次打开应用时 lifd SIGCONT + 写 RESUME 事件 →
/// 本函数返回，应用继续主循环（视觉恢复由 launcher 帧快照保证）。
///
/// lifd 不可用（/data/life 缺失）时回退旧行为：exit(1) 回到桌面。
pub fn lc_home() -> bool {
    if !lc_init() {
        super::print("[lc] lifd 未就绪，回退退出(1)\n");
        unsafe { super::exit(1); }
    }
    lc_write_cmd(b"BG");
    // 等待 RESUME；期间 PAUSE/SUSPEND 事件会被 lc_poll 分发（SIGSTOP 前的宽限窗）
    loop {
        let ev = lc_poll();
        if ev.kind == EV_RESUME {
            return true;
        }
        if !lc_alive() {
            // lifd 消失（异常退出）：避免永久挂起，回退到桌面
            unsafe { super::exit(1); }
        }
        super::sleep_ms(30);
    }
}

/// Recents 手势：后台化并进入最近任务视图（launcher 侧）；恢复后返回。
pub fn lc_go_recents() -> bool {
    if !lc_init() {
        unsafe { super::exit(2); }
    }
    lc_write_cmd(b"RECENTS");
    loop {
        let ev = lc_poll();
        if ev.kind == EV_RESUME {
            return true;
        }
        if !lc_alive() {
            unsafe { super::exit(2); }
        }
        super::sleep_ms(30);
    }
}

/// 判断 lifd 是否仍存活：/data/life 目录可打开即视为存活（轻量探测）。
fn lc_alive() -> bool {
    let fd = unsafe { super::open(LC_DIR, super::O_RDONLY) };
    if fd >= 0 {
        unsafe { super::syscall1(super::SYS_CLOSE, fd as usize) };
        true
    } else {
        false
    }
}

/// 统一处理系统导航动作：
/// - Back → 退出应用（exit(0)）
/// - Home → 后台化（阻塞至恢复后返回 true）
/// - Recents → 后台化+最近任务（恢复后返回 true）
/// - None → 返回 false（无事发生）
pub fn lc_handle_nav(a: super::NavAction) -> bool {
    match a {
        super::NavAction::Back => {
            unsafe { super::exit(0); }
        }
        super::NavAction::Home => lc_home(),
        super::NavAction::Recents => lc_go_recents(),
        super::NavAction::None => false,
    }
}

/// 申请后台任务（类型见 BGT_*；dur_ms 期望时长）。返回是否已提交审核
/// （最终审核结果经 BG_TASK_EXPIRED / 事件反馈）。
pub fn lc_request_bg_task(ty: u8, dur_ms: u32) -> bool {
    if !lc_init() {
        return false;
    }
    let mut cmd = [0u8; 24];
    let n = fmt_bg_cmd(&mut cmd, ty, dur_ms);
    lc_write_cmd(&cmd[..n])
}

fn fmt_bg_cmd(out: &mut [u8; 24], ty: u8, dur_ms: u32) -> usize {
    // "BTASK <ty> <dur_ms>"
    out[..6].copy_from_slice(b"BTASK ");
    let mut n = 6;
    n = push_dec(out, n, ty as u64);
    if n < out.len() {
        out[n] = b' ';
        n += 1;
    }
    n = push_dec(out, n, dur_ms as u64);
    n
}

/// 提前释放后台任务。
pub fn lc_end_bg_task() -> bool {
    if !lc_init() {
        return false;
    }
    lc_write_cmd(b"BTASK_END")
}

/// 保存 UI 状态到 SFS（/data/state/<name>.bin）。返回是否写盘成功。
pub fn lc_save_state(data: &[u8]) -> bool {
    unsafe {
        let mut path = [0u8; 64];
        let n = mk_state_path(&mut path);
        let fd = super::open(
            core::str::from_utf8(&path[..n]).unwrap_or(""),
            super::O_WRONLY | 0x40 | 0x200,
        );
        if fd < 0 {
            return false;
        }
        let r = super::syscall3(super::SYS_WRITE, fd as usize, data.as_ptr() as usize, data.len());
        super::syscall1(super::SYS_CLOSE, fd as usize);
        (r as isize) == data.len() as isize
    }
}

/// 恢复此前保存的 UI 状态。返回读取字节数（0 = 无状态）。
pub fn lc_restore_state(buf: &mut [u8]) -> usize {
    unsafe {
        let mut path = [0u8; 64];
        let n = mk_state_path(&mut path);
        let fd = super::open(core::str::from_utf8(&path[..n]).unwrap_or(""), super::O_RDONLY);
        if fd < 0 {
            return 0;
        }
        let r = super::syscall3(super::SYS_READ, fd as usize, buf.as_mut_ptr() as usize, buf.len())
            as isize;
        super::syscall1(super::SYS_CLOSE, fd as usize);
        if r <= 0 {
            // ★ 必须按 isize 判负：read 失败返回 -errno，usize 原值是 usize::MAX，
            // `r <= 0` 拦不住 → 把全零/陈旧缓冲当有效状态恢复。
            0
        } else {
            (r as usize).min(buf.len())
        }
    }
}

fn mk_state_path(out: &mut [u8; 64]) -> usize {
    unsafe {
        let p = b"/data/state/";
        out[..p.len()].copy_from_slice(p);
        let mut n = p.len();
        let name = &LC_NAME[..LC_NAME_LEN];
        let k = name.len().min(24);
        out[n..n + k].copy_from_slice(&name[..k]);
        n += k;
        let suf = b".bin";
        out[n..n + 4].copy_from_slice(suf);
        n + 4
    }
}

/// 本地推送：提交一条通知到 pushd（写入 /data/push/in/<pid>_<seq>）。
/// pushd 分配 id 并入队，随后经 lifd 投递（NOTIFY 事件）。
pub fn lc_notify_local(title: &[u8], body: &[u8]) -> bool {
    unsafe {
        let mut path = [0u8; 64];
        let mut n = 0usize;
        let p = LC_PUSH_IN_DIR.as_bytes();
        out_copy(&mut path, &mut n, p);
        out_push(&mut path, &mut n, b'/');
        // <pid>_<seq>：seq 取当前时间戳低位，保证同进程多次通知文件名不同
        n = push_dec(&mut path, n, LC_PID as u64);
        out_push(&mut path, &mut n, b'_');
        n = push_dec(&mut path, n, (super::now_us() & 0xFFFF) as u64);
        let fd = super::open(
            core::str::from_utf8(&path[..n]).unwrap_or(""),
            super::O_WRONLY | 0x40 | 0x200,
        );
        if fd < 0 {
            return false;
        }
        // 内容：<title>|<body>（一行，≤ 160 字节）
        let mut payload = [0u8; 160];
        let t = title.len().min(60);
        payload[..t].copy_from_slice(&title[..t]);
        let mut pl = t;
        if pl < payload.len() {
            payload[pl] = b'|';
            pl += 1;
        }
        let b = body.len().min(96);
        payload[pl..pl + b].copy_from_slice(&body[..b]);
        pl += b;
        let r = super::syscall3(super::SYS_WRITE, fd as usize, payload.as_ptr() as usize, pl);
        super::syscall1(super::SYS_CLOSE, fd as usize);
        (r as isize) == pl as isize
    }
}

fn out_copy(out: &mut [u8], n: &mut usize, s: &[u8]) {
    let k = s.len().min(out.len().saturating_sub(*n));
    out[*n..*n + k].copy_from_slice(&s[..k]);
    *n += k;
}

fn out_push(out: &mut [u8], n: &mut usize, c: u8) {
    if *n < out.len() {
        out[*n] = c;
        *n += 1;
    }
}

fn push_dec(out: &mut [u8], mut n: usize, mut v: u64) -> usize {
    if v == 0 {
        if n < out.len() {
            out[n] = b'0';
            n += 1;
        }
        return n;
    }
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    while v > 0 {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    let d = tmp.len() - i;
    if n + d <= out.len() {
        out[n..n + d].copy_from_slice(&tmp[i..]);
    }
    n + d
}
