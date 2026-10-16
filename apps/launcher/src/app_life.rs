// launcher × lifd 集成：非阻塞应用生命周期管理。
//
// 旧模型：launcher fork 应用后阻塞在 waitpid，应用退出才回到桌面
// （切应用 = 杀旧重启）。
// 新模型：launcher 与 lifd 协作，应用后台化（SIGSTOP）而非退出：
//   - 启动完成 → 进入前台态（FOREGROUND）：launcher 不渲染、不读输入，
//     只轮询 wait4(WNOHANG)（应用退出）与 /data/life/launcher_evt
//     （lifd 事件：BG / KILLED / EXITED）。
//   - BG 事件 → 快照应用末帧到 APP_SNAP → 返回动画 → 桌面。
//   - 用户点击存活应用 → 恢复 APP_SNAP → Resume 缩放动画（应用仍停止）→
//     动画完成时向 lifd 发 "FG <pid>"（SIGCONT + RESUME 事件）→ 应用继续。
//   - LMK/异常退出 → wait4 回收僵尸 → 清状态（桌面已在显示，无动画）。
//
// 与 lifd 的文件协议（docs/lifecycle.md §2.3）：
//   reg_<name>         launcher 写（应用启动登记 "pid FG"）
//   cmd_<launcher_pid> launcher 写（FG <pid> / EXITED <name> <pid>）
//   launcher_evt       lifd → launcher（BG/FG/KILLED/EXITED）

use super::*;

// ==================== 协议常量 ====================

const LC_DIR: &str = "/data/life";
const REG_PREFIX: &str = "/data/life/reg_";
const CMD_PREFIX: &str = "/data/life/cmd_";
const LAUNCHER_EVT: &str = "/data/life/launcher_evt";

// 生命周期所需系统调用（AArch64 asm-generic）。
const SYS_GETPID: usize = 172;
const SYS_UNLINKAT: usize = 35;
const SYS_RENAMEAT: usize = 38;

/// WNOHANG（wait4 options）。
const WNOHANG: usize = 0x1;

// ==================== launcher 侧应用状态 ====================

pub const ST_NONE: u8 = 0;
pub const ST_LAUNCHING: u8 = 1;
pub const ST_FOREGROUND: u8 = 2;
pub const ST_BACKGROUND: u8 = 3;
pub const ST_RESUMING: u8 = 4;

static mut APP_STATE: u8 = ST_NONE;
static mut APP_PID: isize = -1;
static mut APP_IDX: usize = usize::MAX;
/// lifd 是否可用（/data/life 存在）。
static mut LIFD_READY: bool = false;
/// 后台化原因（RECENTS 时返回动画结束后打开最近任务面板）。
static mut BG_RECENTS: bool = false;
/// 应用末帧快照（背景化时保存；恢复时回填 fb0 并作为 Resume 动画源）。
static mut APP_SNAP: [u32; DISPLAY_W * DISPLAY_H] = [0u32; DISPLAY_W * DISPLAY_H];
static mut SNAP_VALID: bool = false;
/// launcher 自身 pid（写命令信箱用）。
static mut SELF_PID: isize = -1;
/// 事件缓冲。
static mut EVT_BUF: [u8; 128] = [0; 128];

// ==================== 初始化 / 探测 ====================

/// 探测 lifd 是否可用（目录存在即视为已启动）。
pub fn lifd_init() -> bool {
    unsafe {
        if SELF_PID < 0 {
            SELF_PID = syscall1(SYS_GETPID, 0) as isize;
        }
        let fd = open(LC_DIR, O_RDONLY);
        if fd >= 0 {
            syscall1(SYS_CLOSE, fd as usize);
            LIFD_READY = true;
        }
        LIFD_READY
    }
}

/// 启动应用时登记（fork 成功后由 start_launch 调用）。
pub fn lifd_register(app_idx: usize, pid: isize) {
    if !lifd_init() || pid <= 0 {
        return;
    }
    let app = &APPS[app_idx];
    // reg_<key> = "<pid> FG"
    let mut path = [0u8; PATH_CAP];
    let n = build_path(&mut path, REG_PREFIX, app.key.as_bytes());
    let mut content = [0u8; 32];
    let mut c = 0usize;
    c += push_dec(&mut content, c, pid as u64);
    content[c] = b' ';
    c += 1;
    content[c] = b'F';
    c += 1;
    content[c] = b'G';
    c += 1;
    unsafe {
        write_file(
            core::str::from_utf8(&path[..n]).unwrap_or(""),
            &content[..c],
        );
    }
}

/// 启动动画完成：进入前台管理态（不再阻塞 waitpid）。
pub fn launch_complete(app_idx: usize, pid: isize) {
    unsafe {
        // fork 失败或应用在动画窗口内已被 KILLED/EXITED 事件回收（僵尸已被
        // 收走，waitpid 返回 -ECHILD）：不能无条件进入前台态——否则 APP_PID
        // 指向死 pid，主循环永远轮询前台事件、不再渲染桌面（整体假死）。
        if pid <= 0 {
            APP_STATE = ST_NONE;
            APP_PID = -1;
            SNAP_VALID = false;
            NEEDS_REDRAW = true;
            return;
        }
        APP_STATE = ST_FOREGROUND;
        APP_PID = pid;
        APP_IDX = app_idx;
        SNAP_VALID = false;
        // 应用可能在动画期间已退出（如 execve 失败）：立即回收
        let mut status = 0i32;
        let r = waitpid_nohang(pid, &mut status);
        if r == pid {
            handle_app_exit(app_idx, pid, status);
        } else if r < 0 {
            // 子进程已被事件路径收走：按退出处理回到桌面
            APP_STATE = ST_NONE;
            APP_PID = -1;
            SNAP_VALID = false;
            NEEDS_REDRAW = true;
        }
    }
}

/// 启动动画播放中（fork 已发生，应用尚未成为前台）。
pub fn set_launching(app_idx: usize, pid: isize) {
    unsafe {
        APP_STATE = ST_LAUNCHING;
        APP_PID = pid;
        APP_IDX = app_idx;
        SNAP_VALID = false;
    }
}

/// 取消启动（fly-away）：应用子进程已被回收，通知 lifd 并复位状态。
pub fn cancel_launch(app_idx: usize, pid: isize) {
    unsafe {
        if APP_STATE == ST_LAUNCHING && APP_PID == pid {
            APP_STATE = ST_NONE;
            APP_PID = -1;
        }
        report_exited(app_idx, pid);
    }
}

// ==================== 状态查询 ====================

/// 应用前台/恢复中：launcher 应跳过桌面渲染与输入读取。
pub fn app_active() -> bool {
    unsafe { APP_STATE == ST_FOREGROUND || APP_STATE == ST_RESUMING }
}

/// 恢复转场播放中（应用仍 SIGSTOP；launcher 渲染动画帧）。
pub fn in_resuming() -> bool {
    unsafe { APP_STATE == ST_RESUMING }
}

/// 返回动画完成（由 app_launch::motion_advance 调用）：
/// 后台化原因为 RECENTS 时打开最近任务面板。
/// （壁纸偏好同步已挪到 start_return：转场开始时预渲染桌面，
/// 结束后无需再整屏重绘 —— 原路径会拖出 50~150ms 的退出尾巴。）
pub fn return_animation_done() {
    unsafe {
        if BG_RECENTS {
            BG_RECENTS = false;
            recents_open();
            SHADE_OPEN = false;
        }
    }
}

/// 返回转场被新启动打断（app_launch::start_launch 连续启动路径）：
/// return_animation_done 不会再触发，这里清掉其挂起状态 —— BG_RECENTS
/// 若残留，会在下次返回转场完成时误开最近任务面板。
pub fn return_interrupted() {
    unsafe {
        BG_RECENTS = false;
    }
}

// ==================== 主循环挂钩 ====================

/// 前台期间由主循环调用；返回 true 表示发生了需要渲染的状态变化
/// （返回动画开始 / 应用退出），主循环应继续正常渲染路径。
///
/// 前台期间 launcher 不渲染、不读输入（应用独占 fb0 与输入设备）。
pub fn poll_foreground() -> bool {
    unsafe {
        if APP_STATE != ST_FOREGROUND {
            return false;
        }
        // 1) 应用已退出（Back 键 / 崩溃）→ 回收 + 返回动画
        let mut status = 0i32;
        let r = waitpid_nohang(APP_PID, &mut status);
        if r == APP_PID {
            let idx = APP_IDX;
            let pid = APP_PID;
            APP_STATE = ST_NONE;
            APP_PID = -1;
            handle_app_exit(idx, pid, status);
            return true;
        }
        // 2) lifd 事件：BG（后台化）→ 快照 + 返回动画；KILLED/EXITED → 回收
        let got = read_launcher_evt();
        if got > 0 {
            return handle_launcher_evt(got);
        }
        false
    }
}

/// 桌面期间也轮询 lifd 事件（LMK 击杀后台应用的清理）。
/// 返回 true 表示需要重绘。
pub fn poll_background_events() -> bool {
    unsafe {
        if APP_STATE == ST_FOREGROUND {
            return false;
        }
        // ★ 后台应用被外部杀死（shell kill -9 / 其它 root 进程）时无人回收：
        // launcher 只在前台态 waitpid；lifd 的 alive()=kill(pid,0) 对 Zombie
        // 恒为真（process_exists 对 Zombie 返回 true）→ 双方互等，僵尸占
        // 内核进程槽 + lifd 注册表占 MAX_APPS 槽，泄漏几次后全部新应用
        // 登记失败，生命周期子系统整体失效。后台态补一次非阻塞 waitpid：
        // 回收成功即清 launcher 侧状态并通知 lifd 摘除注册（不走
        // handle_app_exit —— 它假设 fb0 仍是应用末帧并播放返回动画，
        // 后台被杀时屏上已是桌面）。
        if APP_STATE == ST_BACKGROUND && APP_PID > 0 {
            let mut status = 0i32;
            let r = waitpid_nohang(APP_PID, &mut status);
            if r == APP_PID {
                let idx = APP_IDX;
                let pid = APP_PID;
                APP_STATE = ST_NONE;
                APP_PID = -1;
                SNAP_VALID = false;
                print("[launcher] 后台应用 '");
                print(APPS[idx].name);
                print("' 被终止，已回收\n");
                report_exited(idx, pid);
                return true;
            }
        }
        let got = read_launcher_evt();
        if got > 0 {
            return handle_launcher_evt(got);
        }
        false
    }
}

/// 读 launcher_evt（rename 取走后读，读完删除）。返回内容长度（0 = 无事件）。
///
/// 取走与读取之间 lifd 新发布的事件落在重生的 launcher_evt 上，不会被
/// 本次 unlink 吞掉（此前「读 → unlink」与 lifd 的 O_TRUNC 覆盖写存在
/// 交错丢事件窗口；lifd 侧已同步改为 .new + rename 发布）。
fn read_launcher_evt() -> usize {
    unsafe {
        // 原子改名取走。
        let mut taken = [0u8; PATH_CAP];
        let tn = LAUNCHER_EVT.len().min(PATH_CAP - 3);
        taken[..tn].copy_from_slice(&LAUNCHER_EVT.as_bytes()[..tn]);
        taken[tn..tn + 2].copy_from_slice(b".r");
        let mut cur = [0u8; PATH_CAP];
        cur[..tn].copy_from_slice(&LAUNCHER_EVT.as_bytes()[..tn]);
        let r = syscall4(
            SYS_RENAMEAT,
            AT_FDCWD,
            cur.as_ptr() as usize,
            AT_FDCWD,
            taken.as_ptr() as usize,
        ) as isize;
        if r != 0 {
            return 0;
        }
        let tpath = core::str::from_utf8(&taken[..tn + 2]).unwrap_or("");
        let fd = open(tpath, O_RDONLY);
        if fd < 0 {
            // 改名成功、open 失败：改名回原名让下轮重取（事件不丢）。
            syscall4(
                SYS_RENAMEAT,
                AT_FDCWD,
                taken.as_ptr() as usize,
                AT_FDCWD,
                cur.as_ptr() as usize,
            );
            return 0;
        }
        let n = syscall3(
            SYS_READ,
            fd as usize,
            EVT_BUF.as_mut_ptr() as usize,
            EVT_BUF.len(),
        ) as isize;
        syscall1(SYS_CLOSE, fd as usize);
        unlink_path(tpath);
        if n <= 0 {
            0
        } else {
            (n as usize).min(EVT_BUF.len())
        }
    }
}

/// 处理一条 lifd 事件。返回是否需重绘。
fn handle_launcher_evt(len: usize) -> bool {
    unsafe {
        let s = &EVT_BUF[..len];
        if s.starts_with(b"BG ") {
            // BG <name> <pid> <reason>
            let pid = parse_pid_after(s, 3);
            if pid == APP_PID {
                // 快照应用末帧（此刻 fb0 仍是应用最后绘制的帧）
                snapshot_frame();
                SNAP_VALID = true;
                // "RECENTS" 是 7 字节（lifd k2 = reason.len().min(8)）——
                // windows(8) 的 8 字节窗口与 7 字节模式永不相等，恒 false，
                // 从最近任务手势返回后返回动画结束不会打开最近任务面板。
                BG_RECENTS = s.windows(7).any(|w| w == b"RECENTS");
                let idx = APP_IDX;
                APP_STATE = ST_BACKGROUND;
                RECENT_APP = idx; // 最近任务列表首位
                                  // 返回动画（start_return 内部把 fb0 拷贝到 FRAME_BUF 并启动 Phase::Return）
                // B10：停止占位页呼吸（应用后台化，占位态随之失效）。
                app_launch::placeholder_stop();
                app_launch::start_return(idx);
                return true;
            }
        } else if s.starts_with(b"KILLED ") || s.starts_with(b"EXITED ") {
            // KILLED/EXITED <name> <pid>：LMK 或后台异常退出 → 回收僵尸
            let pid = parse_pid_after(s, 8);
            if pid == APP_PID {
                let mut status = 0i32;
                let r = waitpid_nohang(APP_PID, &mut status);
                let _ = r;
                // 启动动画窗口内被杀：同步清除 app_launch 侧的 LAUNCH_PID，
                // 防止动画完成时 launch_complete 复活已回收的 pid（假死）
                if APP_STATE == ST_LAUNCHING {
                    app_launch::clear_launch_pid();
                }
                // B10：停止占位页呼吸（呼吸基线属于已失效的前台等待态，
                // 残留会在下次前台期闪现旧应用骨架）。
                app_launch::placeholder_stop();
                APP_STATE = ST_NONE;
                APP_PID = -1;
                SNAP_VALID = false;
            }
        }
        false
    }
}

/// 事件内容 "PREFIX <name> <pid>"：返回 pid（-1 解析失败）。
fn parse_pid_after(s: &[u8], start: usize) -> isize {
    // 找到最后一个空白后的数字段（pid 在行尾）
    let mut i = start;
    while i < s.len() && s[i] != b' ' {
        i += 1;
    }
    while i < s.len() && s[i] == b' ' {
        i += 1;
    }
    let mut v: i64 = 0;
    let mut any = false;
    while i < s.len() && s[i] >= b'0' && s[i] <= b'9' {
        v = v * 10 + (s[i] - b'0') as i64;
        any = true;
        i += 1;
    }
    if any {
        v as isize
    } else {
        -1
    }
}

// ==================== 恢复（用户点击存活应用） ====================

/// 应用是否存活（后台/挂起，等待恢复）。
pub fn app_alive(app_idx: usize) -> bool {
    unsafe { APP_STATE == ST_BACKGROUND && APP_IDX == app_idx && APP_PID > 0 }
}

/// 恢复后台应用：恢复快照 → Resume 缩放动画（应用仍 SIGSTOP）→
/// 动画完成时经 `resume_animation_done` 通知 lifd（SIGCONT）。
pub fn resume_app(app_idx: usize) -> bool {
    resume_app_at(app_idx, None)
}

/// 同 `resume_app`，但放大转场以 `anchor`（最近任务卡片中心等）为起点；
/// `None` 时回退图标原位。
pub fn resume_app_at(app_idx: usize, anchor: Option<(i32, i32)>) -> bool {
    unsafe {
        if APP_STATE != ST_BACKGROUND || APP_IDX != app_idx || APP_PID <= 0 {
            return false;
        }
        // B10：占位页呼吸态兜底清除（正常路径已在 BG 事件停止；此处防御
        // 残留激活态在恢复后的前台期闪现旧骨架）。
        app_launch::placeholder_stop();
        if !SNAP_VALID {
            // 无快照（异常）：直接通知 lifd 恢复
            request_fg(APP_PID);
            APP_STATE = ST_FOREGROUND;
            return true;
        }
        // 快照 → FRAME_BUF（Resume 动画缩放源）
        core::ptr::copy_nonoverlapping(APP_SNAP.as_ptr(), frame_buf_ref(), DISPLAY_W * DISPLAY_H);
        // 动画起点：锚点（最近任务卡片）优先，否则图标原位放大到全屏
        // （与启动动画方向一致，源为应用快照）
        let (cx, cy) = anchor.unwrap_or_else(|| icon_center(app_idx));
        let need = cx
            .max(DISPLAY_W as i32 - cx)
            .max(cy)
            .max(DISPLAY_H as i32 - cy);
        let cover_px = (need * 2 + 16) as u32;
        let target = (cover_px * SCALE_1 / ICON_SIZE as u32).max(SCALE_1 + 1);
        let m = &mut MOTION;
        m.phase = Phase::Resume;
        m.app = app_idx;
        m.cx = cx;
        m.cy = cy;
        m.launch_target = target;
        m.cur_scale = SCALE_1;
        m.cur_fade = 0;
        m.t0 = now_us();
        m.launch_t = now_us();
        m.launch_spring = Spring::launch();
        // Spring::launch() 的 pos/target 均为 0：不赋值的话 step_ms 首调即
        // settled，恢复转场零帧播完（返回中途点后台应用图标时尤其显眼），
        // 屏幕停在旧帧直到被恢复应用 SIGCONT 后自行重绘。与 start_launch
        // 的写法对齐：从图标原位（SCALE_1）放大到覆盖全屏。
        m.launch_spring.pos = SCALE_1 as f32;
        m.launch_spring.target = target as f32;
        APP_STATE = ST_RESUMING;
        NEEDS_REDRAW = true;
        true
    }
}

/// Resume 动画完成（由 app_launch::motion_advance 调用）：通知 lifd 恢复。
pub fn resume_animation_done() {
    unsafe {
        if APP_STATE == ST_RESUMING {
            request_fg(APP_PID);
            APP_STATE = ST_FOREGROUND;
        }
    }
}

/// 命令序号（每消息一文件的唯一名）。
static mut CMD_SEQ: u64 = 0;

/// 发布命令到 cmd_<launcher_pid>_<seq>（写 .tmp 后 rename 发布）。
/// 每消息一文件消除「O_TRUNC 覆盖 × lifd 读后删除」的丢命令竞态；
/// lifd 侧目录扫描 + parse_dec 到非数字停止，命名兼容零改动。
fn publish_cmd(content: &[u8]) {
    unsafe {
        CMD_SEQ = CMD_SEQ.wrapping_add(1);
        let mut base = [0u8; PATH_CAP];
        let mut n = 0usize;
        copy_bytes(&mut base, &mut n, CMD_PREFIX.as_bytes());
        n = push_dec(&mut base, n, SELF_PID as u64);
        if n + 1 < base.len() {
            base[n] = b'_';
            n += 1;
        }
        n = push_dec(&mut base, n, CMD_SEQ);
        let mut tmp = [0u8; PATH_CAP];
        tmp[..n].copy_from_slice(&base[..n]);
        tmp[n..n + 4].copy_from_slice(b".tmp");
        if !write_file(core::str::from_utf8(&tmp[..n + 4]).unwrap_or(""), content) {
            return;
        }
        let mut newp = [0u8; PATH_CAP];
        newp[..n].copy_from_slice(&base[..n]);
        syscall4(
            SYS_RENAMEAT,
            AT_FDCWD,
            tmp.as_ptr() as usize,
            AT_FDCWD,
            newp.as_ptr() as usize,
        );
    }
}

/// 向 lifd 请求把 pid 置为前台（cmd_<launcher_pid>_<seq> = "FG <pid>"）。
fn request_fg(pid: isize) {
    if pid <= 0 {
        return;
    }
    unsafe {
        let mut content = [0u8; 24];
        let mut c = 0usize;
        copy_bytes(&mut content, &mut c, b"FG ");
        c = push_dec(&mut content, c, pid as u64);
        publish_cmd(&content[..c]);
    }
}

/// 报告应用已退出（cmd_<launcher_pid>_<seq> = "EXITED <key> <pid>"）。
fn report_exited(app_idx: usize, pid: isize) {
    if !unsafe { LIFD_READY } || pid <= 0 {
        return;
    }
    unsafe {
        let app = &APPS[app_idx];
        let mut content = [0u8; 48];
        let mut c = 0usize;
        copy_bytes(&mut content, &mut c, b"EXITED ");
        copy_bytes(&mut content, &mut c, app.key.as_bytes());
        content[c] = b' ';
        c += 1;
        c = push_dec(&mut content, c, pid as u64);
        publish_cmd(&content[..c]);
    }
}

// ==================== 应用退出处理 ====================

/// 应用退出（前台退出或启动失败）：处理退出码 + 返回动画 + 通知 lifd。
fn handle_app_exit(app_idx: usize, pid: isize, status: i32) {
    let app = &APPS[app_idx];
    unsafe {
        // B10：停止占位页呼吸（应用退出，返回动画即将接管显存）。
        app_launch::placeholder_stop();
        let mut code = 0u32;
        if status != 0 {
            code = ((status >> 8) & 0xFF) as u32;
        }
        // 退出消息无条件打印（Back=0 同样输出）：生命周期回归脚本以该行
        // 判定应用已退出，静默会使宿主测试无法区分「已退出」与「假死」。
        print("[launcher] 应用 '");
        print(app.name);
        print("' 已退出，退出码=");
        print_dec(code as u32);
        print("\n");
        // 退出码约定：0 普通返回；1 回桌面首页；2 打开最近任务；
        // 127 = execve 失败（launch_child 子进程约定），提示启动失败。
        if code == 127 {
            show_toast(txt("App failed to start", "应用启动失败"));
        }
        match code {
            1 => {
                CURRENT_PAGE = 0;
                SHADE_OPEN = false;
                RECENTS_OPEN = false;
            }
            2 => {
                recents_open();
                SHADE_OPEN = false;
            }
            _ => {}
        }
        // 通知 lifd 移除注册（lifd 不可用时无操作）
        report_exited(app_idx, pid);
        // 应用覆盖了桌面（应用自己画 fb），退出后以缩放转场回到桌面。
        app_launch::start_return(app_idx);
        NEEDS_REDRAW = true;
    }
}

// ==================== 帧快照 ====================

/// 拷贝 fb0 当前内容到 APP_SNAP（应用末帧）。
pub fn snapshot_frame() {
    unsafe {
        if FB.is_null() {
            return;
        }
        let w = FB_W.min(DISPLAY_W);
        let h = FB_H.min(DISPLAY_H);
        for y in 0..h {
            core::ptr::copy_nonoverlapping(
                FB.add(y * FB_STRIDE_PX),
                APP_SNAP.as_mut_ptr().add(y * DISPLAY_W),
                w,
            );
        }
    }
}

fn frame_buf_ref() -> *mut u32 {
    // FRAME_BUF 位于 app_launch 模块（private static）。
    // 通过一个中转访问：在 app_launch 提供 snapshot_into_frame()。
    app_launch::frame_buf_mut()
}

// ==================== 工具 ====================

/// 构建 "<prefix><name>" 路径。
fn build_path(out: &mut [u8; PATH_CAP], prefix: &str, name: &[u8]) -> usize {
    let mut n = 0usize;
    copy_bytes(out, &mut n, prefix.as_bytes());
    // ★ saturating_sub(n) - 1 在剩余空间为 0 时回绕成 usize::MAX →
    // out[n..n+k] 越界 panic；改为对 n+1（预留空间）做饱和减。
    let k = name.len().min(out.len().saturating_sub(n + 1));
    out[n..n + k].copy_from_slice(&name[..k]);
    n + k
}

fn copy_bytes(out: &mut [u8], n: &mut usize, s: &[u8]) {
    let k = s.len().min(out.len().saturating_sub(*n));
    out[*n..*n + k].copy_from_slice(&s[..k]);
    *n += k;
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

/// wait4(WNOHANG)：有已退出子进程立即回收（返回 pid），否则返回 0。
/// ABI：wait4(pid, status, options, rusage) —— options 在第 3 参数（x2）。
fn waitpid_nohang(pid: isize, status: &mut i32) -> isize {
    unsafe {
        syscall4(
            SYS_WAIT4,
            pid as usize,
            status as *mut i32 as usize,
            WNOHANG,
            0,
        ) as isize
    }
}

fn unlink_path(path: &str) {
    let mut p = [0u8; PATH_CAP];
    let n = path.len().min(PATH_CAP - 1);
    p[..n].copy_from_slice(&path.as_bytes()[..n]);
    p[n] = 0;
    unsafe {
        syscall3(SYS_UNLINKAT, AT_FDCWD, p.as_ptr() as usize, 0);
    }
}
