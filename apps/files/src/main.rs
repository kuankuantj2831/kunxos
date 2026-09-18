// KunxOS 文件管理器应用（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由 launcher 经 fork + execve 拉起（/bin/files）。职责：
//   1. 目录浏览：列出文件/文件夹（文件夹优先 + 字母序），图标区分类型，
//      路径栏显示当前路径、点击分段可跳转上级
//   2. 基础操作：新建文件夹、重命名、删除、复制、移动（对话框确认 +
//      错误提示）；大文件/目录复制在子进程中执行并显示进度条（不阻塞 UI）
//   3. 文件打开：按扩展名映射关联应用（.wav/.mp3 → /bin/music，
//      .jpg/.png → /bin/gallery，文本 → 内置查看器）
//   4. 存储显示：当前文件系统总空间 / 已用 / 剩余（statfs syscall）
//
// 说明：framebuffer 由内核启动时初始化，应用直接 open/mmap /dev/fb0 覆盖桌面
// 全屏绘制；退出后桌面由 launcher 重绘。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

#[path = "../../../shared/app_kit.rs"]
mod kit;

use kit::*;

#[path = "../../../shared/motion.rs"]
mod motion;

use motion::Easing;

// 共享对话框组件（VIVO OriginOS 风格：居中白卡 + 细分割线 + 文字按钮）。
#[path = "../../../shared/dialog.rs"]
mod dialog;

// ==================== 系统调用号（AArch64 asm-generic） ====================

const SYS_CLONE: usize = 220;
const SYS_EXECVE: usize = 221;
const SYS_WAIT4: usize = 260;
const SYS_GETPID: usize = 172;
const SYS_KILL: usize = 129;
const SYS_MKDIRAT: usize = 34;
const SYS_UNLINKAT: usize = 35;
const SYS_RENAMEAT: usize = 38;
const SYS_STATFS: usize = 43;
const SYS_GETDENTS64: usize = 61;
const SYS_LSEEK: usize = 62;
const SYS_NEWFSTATAT: usize = 79;

const SIGCHLD: usize = 17;

// ==================== 常量 ====================

/// getdents64 d_type 值。
const DT_DIR: u8 = 4;
const DT_REG: u8 = 8;
const DT_LNK: u8 = 10;

const MAX_ENTRIES: usize = 96;
const MAX_NAME: usize = 31;
const MAX_PATH: usize = 160;
const VIEW_CAP: usize = 5120;

const ROW_H: i32 = 40;
const PATH_BAR_Y: i32 = TITLE_H + 2;
const PATH_BAR_H: i32 = 28;
const STORE_Y: i32 = PATH_BAR_Y + PATH_BAR_H + 4;
const STORE_H: i32 = 22;
const LIST_TOP: i32 = STORE_Y + STORE_H + 4;
const TOOL_H: i32 = 58;
const TOOL_Y: i32 = NAV_Y - TOOL_H - 4;
const LIST_BOTTOM: i32 = TOOL_Y - 6;

/// 界面模式。
const MODE_BROWSE: u8 = 0;
const MODE_VIEW: u8 = 1;
const MODE_INPUT: u8 = 2;
const MODE_CONFIRM: u8 = 3;
const MODE_PROGRESS: u8 = 4;

/// 对话框动作。
const ACT_NEW_FOLDER: u8 = 0;
const ACT_RENAME: u8 = 1;
const ACT_DELETE: u8 = 2;

/// 剪贴板操作类型。
const CLIP_NONE: u8 = 0;
const CLIP_COPY: u8 = 1;
const CLIP_MOVE: u8 = 2;

const PROG_COPY: u8 = 0;
const PROG_MOVE: u8 = 1;

/// 后台复制进度文件（子进程写，父进程轮询）。
const CP_STATE: &[u8] = b"/var/run/files_cp.st\0";

// ==================== 颜色 ====================

/// 页面基底背景（浅色渐变：顶部白 → 底部极浅灰）；也被图标镂空与
/// 选中勾复用（作为“透明底”色）。
const C_BG: u32 = 0x00_F7_F8_FA;
const C_FOLDER: u32 = 0x00_E8_A0_4C;
const C_FILE: u32 = 0x00_5B_9B_D5;
const C_LINK: u32 = 0x00_C9_5C_EF;
const C_SEL: u32 = 0x00_35_B0_E6;
const C_DANGER: u32 = 0x00_E0_4A_3A;
/// 彩色玻璃（强调绿/危险红）表面上的文字保持白色。
const C_ON_ACCENT: u32 = 0x00_FF_FF_FF;

// ==================== 状态 ====================

/// 当前目录（绝对路径，NUL 结尾）。
static mut CWD: [u8; 128] = [0; 128];
static mut UI_MODE: u8 = MODE_BROWSE;

// ---- 目录列表 ----
static mut ENT_NAMES: [[u8; MAX_NAME]; MAX_ENTRIES] = [[0; MAX_NAME]; MAX_ENTRIES];
static mut ENT_LEN: [usize; MAX_ENTRIES] = [0; MAX_ENTRIES];
static mut ENT_TYPE: [u8; MAX_ENTRIES] = [0; MAX_ENTRIES];
static mut ENT_SIZE: [u64; MAX_ENTRIES] = [0; MAX_ENTRIES];
static mut ENT_COUNT: usize = 0;
static mut LIST_SCROLL: usize = 0;
/// 当前选中项下标（usize::MAX = 无）。
static mut SEL_IDX: usize = usize::MAX;

// ---- 剪贴板 ----
static mut CLIP_OP: u8 = CLIP_NONE;
static mut CLIP_PATH: [u8; MAX_PATH] = [0; MAX_PATH];
static mut CLIP_LEN: usize = 0;
static mut CLIP_NAME: [u8; MAX_NAME] = [0; MAX_NAME];
static mut CLIP_NAME_LEN: usize = 0;

// ---- 路径栏分段 ----
static mut SEG_X: [usize; 8] = [0; 8];
static mut SEG_W: [usize; 8] = [0; 8];
static mut SEG_OFF: [usize; 8] = [0; 8]; // 段在 CWD 中的起始字节
static mut SEG_N: usize = 0;

// ==================== 页面切换转场（P0-2，settings/sms 同款整页滑动） ====================

/// 离场页整帧快照（2MB BSS，与 BACK_BUF 同级）。
static mut SNAP_BUF: [u32; DISPLAY_W * DISPLAY_H] = [0u32; DISPLAY_W * DISPLAY_H];
/// 转场起始时刻；0 = 无转场。
static mut TRANS_T0: u64 = 0;
/// true = 推入（进入子目录/查看器）；false = 弹出（返回上级）。
static mut TRANS_PUSH: bool = false;
/// 转场时长（鸿蒙页面转场 240ms FastOutSlowIn）。
const TRANS_DUR: u64 = 240_000;

/// 向黑色压暗 `a/255`。
fn shade(c: u32, a: u32) -> u32 {
    let r = ((c >> 16) & 0xFF) * a / 255;
    let g = ((c >> 8) & 0xFF) * a / 255;
    let b = (c & 0xFF) * a / 255;
    (r << 16) | (g << 8) | b
}

/// 把离场页整帧从 BACK_BUF 拷贝到 SNAP_BUF（切换目录/视图前调用）。
fn snapshot_page() {
    unsafe {
        core::ptr::copy_nonoverlapping(
            BACK_BUF.as_ptr(),
            SNAP_BUF.as_mut_ptr(),
            DISPLAY_W * DISPLAY_H,
        );
    }
}

/// 发起转场：记录时刻与方向。
fn begin_transition(push: bool) {
    unsafe {
        TRANS_PUSH = push;
        TRANS_T0 = kit::now_us();
    }
}

/// 转场是否仍在进行（主循环据此逐帧重绘）。
fn transition_active() -> bool {
    unsafe { TRANS_T0 != 0 && kit::now_us() < TRANS_T0 + TRANS_DUR }
}

/// 页面转场合成（行级平移 + 快照补底 + 10px 接缝阴影，settings 同款）。
fn compose_transition(e: f32) {
    if e >= 1.0 {
        return;
    }
    unsafe {
        let w = DISPLAY_W;
        let off = ((1.0 - e) * w as f32) as usize;
        if off == 0 {
            return;
        }
        for y in 0..DISPLAY_H {
            let row = y * w;
            if TRANS_PUSH {
                for x in (off..w).rev() {
                    BACK_BUF[row + x] = BACK_BUF[row + x - off];
                }
                for x in 0..off {
                    BACK_BUF[row + x] = SNAP_BUF[row + x];
                }
                for s in 0..10 {
                    if s < off {
                        let x = off - 1 - s;
                        BACK_BUF[row + x] = shade(BACK_BUF[row + x], (90 - 9 * s) as u32);
                    }
                }
            } else {
                for x in 0..w - off {
                    BACK_BUF[row + x] = BACK_BUF[row + x + off];
                }
                for x in (w - off)..w {
                    BACK_BUF[row + x] = SNAP_BUF[row + x - (w - off)];
                }
                let c = w - off;
                for s in 0..10 {
                    if s < off && c > s {
                        let x = c - 1 - s;
                        BACK_BUF[row + x] = shade(BACK_BUF[row + x], (90 - 9 * s) as u32);
                    }
                }
            }
        }
    }
}

// ---- 输入对话框 ----
static mut INPUT_ACTION: u8 = ACT_NEW_FOLDER;
static mut INPUT_TARG: [u8; MAX_PATH] = [0; MAX_PATH];
static mut INPUT_TARG_LEN: usize = 0;
static mut INPUT_BUF: [u8; MAX_NAME] = [0; MAX_NAME];
static mut INPUT_LEN: usize = 0;

// ---- 确认对话框 ----
static mut CONFIRM_ACTION: u8 = ACT_DELETE;
static mut CONFIRM_PATH: [u8; MAX_PATH] = [0; MAX_PATH];
static mut CONFIRM_PATH_LEN: usize = 0;

// ---- 查看器 ----
static mut VIEW_PATH: [u8; MAX_PATH] = [0; MAX_PATH];
static mut VIEW_LEN: usize = 0;
static mut VIEW_BUF: [u8; VIEW_CAP] = [0; VIEW_CAP];
static mut VIEW_SCROLL: usize = 0;

// ---- 后台操作（进度） ----
static mut PROG_KIND: u8 = PROG_COPY;
static mut PROG_PID: i32 = -1;
static mut PROG_START_US: u64 = 0;
static mut PROG_LABEL: [u8; 32] = [0; 32];
static mut PROG_LABEL_LEN: usize = 0;

// ---- 复制执行缓冲 ----
static mut CP_BUF: [u8; 4096] = [0; 4096];
static mut OP_SRC: [u8; MAX_PATH] = [0; MAX_PATH];
static mut OP_SRC_LEN: usize = 0;
static mut OP_DST: [u8; MAX_PATH] = [0; MAX_PATH];
static mut OP_DST_LEN: usize = 0;
static mut OP_NAME: [u8; MAX_NAME] = [0; MAX_NAME];
static mut OP_NAME_LEN: usize = 0;

// ==================== 系统调用封装 ====================

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

// ==================== 路径工具 ====================

fn nul_len(b: &[u8]) -> usize {
    b.iter().position(|&c| c == 0).unwrap_or(b.len())
}

fn cwd_len() -> usize {
    unsafe { nul_len(&CWD) }
}

/// 把 `base` + "/" + `name` 拼接进 `out`（NUL 结尾），返回长度；过长返回 0。
fn join_into(base: &[u8], name: &[u8], out: &mut [u8]) -> usize {
    let mut off = 0usize;
    for &c in base {
        if off + 1 >= out.len() {
            return 0;
        }
        out[off] = c;
        off += 1;
    }
    if off > 0 && out[off - 1] != b'/' && off + 1 < out.len() {
        out[off] = b'/';
        off += 1;
    }
    for &c in name {
        if off + 1 >= out.len() {
            return 0;
        }
        out[off] = c;
        off += 1;
    }
    out[off] = 0;
    off
}

/// 绝对路径字符串（CWD 前缀，NUL 结尾于静态缓冲）。
fn cwd_nul() -> &'static [u8] {
    unsafe {
        let mut n = 0usize;
        while n < CWD.len() && CWD[n] != 0 {
            n += 1;
        }
        &CWD[..(n + 1).min(CWD.len())]
    }
}

/// 打开路径（NUL 结尾于 OP_SRC 缓冲；注意会覆盖 OP_SRC）。
unsafe fn open_path(p: &[u8], flags: usize) -> isize {
    let n = p.len().min(OP_SRC.len() - 1);
    OP_SRC[..n].copy_from_slice(&p[..n]);
    OP_SRC[n] = 0;
    syscall3(SYS_OPENAT, AT_FDCWD, OP_SRC.as_ptr() as usize, flags) as isize
}

fn mkdir_path(p: &[u8]) -> usize {
    unsafe {
        let n = p.len().min(OP_SRC.len() - 1);
        OP_SRC[..n].copy_from_slice(&p[..n]);
        OP_SRC[n] = 0;
        syscall3(SYS_MKDIRAT, AT_FDCWD, OP_SRC.as_ptr() as usize, 0o777)
    }
}

fn unlink_path(p: &[u8]) -> usize {
    unsafe {
        let n = p.len().min(OP_SRC.len() - 1);
        OP_SRC[..n].copy_from_slice(&p[..n]);
        OP_SRC[n] = 0;
        syscall3(SYS_UNLINKAT, AT_FDCWD, OP_SRC.as_ptr() as usize, 0)
    }
}

fn rename_path(old: &[u8], new: &[u8]) -> usize {
    unsafe {
        let n = old.len().min(OP_SRC.len() - 1);
        OP_SRC[..n].copy_from_slice(&old[..n]);
        OP_SRC[n] = 0;
        let m = new.len().min(OP_DST.len() - 1);
        OP_DST[..m].copy_from_slice(&new[..m]);
        OP_DST[m] = 0;
        syscall4(
            SYS_RENAMEAT,
            AT_FDCWD,
            OP_SRC.as_ptr() as usize,
            AT_FDCWD,
            OP_DST.as_ptr() as usize,
        )
    }
}

/// stat：返回 (类型码, 大小)。类型码 0 = 失败。
fn stat_path(p: &[u8]) -> (u8, u64) {
    unsafe {
        let n = p.len().min(OP_SRC.len() - 1);
        OP_SRC[..n].copy_from_slice(&p[..n]);
        OP_SRC[n] = 0;
        let mut st = [0u8; 128];
        let r = syscall4(
            SYS_NEWFSTATAT,
            AT_FDCWD,
            OP_SRC.as_ptr() as usize,
            st.as_mut_ptr() as usize,
            0,
        );
        if r != 0 {
            return (0, 0);
        }
        let mode = u32::from_le_bytes([st[16], st[17], st[18], st[19]]);
        let size = i64::from_le_bytes([
            st[48], st[49], st[50], st[51], st[52], st[53], st[54], st[55],
        ]);
        let typ = match mode & 0o170000 {
            0o040000 => DT_DIR,
            0o120000 => DT_LNK,
            0o020000 => 2,
            0o060000 => 6,
            0o010000 => 1,
            _ => DT_REG,
        };
        (typ, size.max(0) as u64)
    }
}

/// statfs：返回 (总字节, 空闲字节)；失败返回 (0,0)。
fn statfs_bytes(p: &[u8]) -> (u64, u64) {
    unsafe {
        let n = p.len().min(OP_SRC.len() - 1);
        OP_SRC[..n].copy_from_slice(&p[..n]);
        OP_SRC[n] = 0;
        let mut st = [0u8; 120];
        let r = syscall2(SYS_STATFS, OP_SRC.as_ptr() as usize, st.as_mut_ptr() as usize);
        if r != 0 {
            return (0, 0);
        }
        let bsize = i64::from_le_bytes([
            st[8], st[9], st[10], st[11], st[12], st[13], st[14], st[15],
        ]) as u64;
        let blocks = i64::from_le_bytes([
            st[16], st[17], st[18], st[19], st[20], st[21], st[22], st[23],
        ]) as u64;
        let bfree = i64::from_le_bytes([
            st[24], st[25], st[26], st[27], st[28], st[29], st[30], st[31],
        ]) as u64;
        (blocks.saturating_mul(bsize), bfree.saturating_mul(bsize))
    }
}

// ==================== 目录加载 ====================

/// 读取目录到 ENT_*：文件夹优先，组内按字节字典序；"." / ".." 固定最前。
fn load_dir(path: &[u8]) {
    unsafe {
        ENT_COUNT = 0;
        LIST_SCROLL = 0;
        let fd = open_path(path, O_RDONLY);
        if fd < 0 {
            show_error("无法打开目录".as_bytes());
            return;
        }
        let mut raw = [0u8; 2048];
        let mut names: [[u8; MAX_NAME]; MAX_ENTRIES] = [[0; MAX_NAME]; MAX_ENTRIES];
        let mut lens: [usize; MAX_ENTRIES] = [0; MAX_ENTRIES];
        let mut types: [u8; MAX_ENTRIES] = [0; MAX_ENTRIES];
        let mut count = 0usize;
        loop {
            let n = syscall3(
                SYS_GETDENTS64,
                fd as usize,
                raw.as_mut_ptr() as usize,
                raw.len(),
            );
            if n as isize <= 0 {
                break;
            }
            let mut off = 0usize;
            while off + 19 <= n as usize {
                let reclen =
                    u16::from_le_bytes([raw[off + 16], raw[off + 17]]) as usize;
                if reclen < 19 || off + reclen > n as usize {
                    break;
                }
                let dtyp = raw[off + 18];
                let name = &raw[off + 19..off + reclen];
                let elen = name.iter().position(|&c| c == 0).unwrap_or(name.len());
                if count < MAX_ENTRIES {
                    names[count][..elen.min(MAX_NAME)].copy_from_slice(&name[..elen.min(MAX_NAME)]);
                    lens[count] = elen.min(MAX_NAME);
                    types[count] = dtyp;
                    count += 1;
                }
                off += reclen;
            }
        }
        syscall1(SYS_CLOSE, fd as usize);

        // 抽出 ".." / "."；其余先文件夹优先排序（同类型内字母序）。
        let mut dots: [[u8; MAX_NAME]; 2] = [[0; MAX_NAME]; 2];
        let mut dot_lens = [0usize; 2];
        let mut dot_count = 0usize;
        let mut rest: [[u8; MAX_NAME]; MAX_ENTRIES] = [[0; MAX_NAME]; MAX_ENTRIES];
        let mut rest_len = [0usize; MAX_ENTRIES];
        let mut rest_typ = [0u8; MAX_ENTRIES];
        let mut rest_n = 0usize;
        for i in 0..count {
            let nm = &names[i][..lens[i]];
            if nm == b".." || nm == b"." {
                if dot_count < 2 {
                    dots[dot_count][..lens[i]].copy_from_slice(nm);
                    dot_lens[dot_count] = lens[i];
                    dot_count += 1;
                }
            } else {
                rest[rest_n][..lens[i]].copy_from_slice(nm);
                rest_len[rest_n] = lens[i];
                rest_typ[rest_n] = types[i];
                rest_n += 1;
            }
        }
        // 插入排序：文件夹优先，同类型按字节序。
        for i in 1..rest_n {
            let mut j = i;
            while j > 0 {
                let a_is_dir = rest_typ[j - 1] == DT_DIR;
                let b_is_dir = rest_typ[j] == DT_DIR;
                let a = &rest[j - 1][..rest_len[j - 1]];
                let b = &rest[j][..rest_len[j]];
                let earlier = if a_is_dir != b_is_dir {
                    a_is_dir
                } else {
                    a <= b
                };
                if earlier {
                    break;
                }
                let al = rest_len[j - 1];
                let bl = rest_len[j];
                let mut ta = [0u8; MAX_NAME];
                let mut tb = [0u8; MAX_NAME];
                ta[..al].copy_from_slice(&rest[j - 1][..al]);
                tb[..bl].copy_from_slice(&rest[j][..bl]);
                rest[j - 1][..bl].copy_from_slice(&tb[..bl]);
                rest_len[j - 1] = bl;
                let tt = rest_typ[j - 1];
                rest_typ[j - 1] = rest_typ[j];
                rest_typ[j] = tt;
                rest[j][..al].copy_from_slice(&ta[..al]);
                rest_len[j] = al;
                j -= 1;
            }
        }
        // 合并。
        count = 0;
        for d in 0..dot_count {
            ENT_NAMES[count][..dot_lens[d]].copy_from_slice(&dots[d][..dot_lens[d]]);
            ENT_LEN[count] = dot_lens[d];
            ENT_TYPE[count] = DT_DIR;
            ENT_SIZE[count] = 0;
            count += 1;
        }
        for i in 0..rest_n {
            ENT_NAMES[count][..rest_len[i]].copy_from_slice(&rest[i][..rest_len[i]]);
            ENT_LEN[count] = rest_len[i];
            ENT_TYPE[count] = rest_typ[i];
            ENT_SIZE[count] = 0;
            count += 1;
        }
        ENT_COUNT = count;
        // 取大小。
        for i in 0..ENT_COUNT {
            let nm = &ENT_NAMES[i][..ENT_LEN[i]];
            if nm == b".." || nm == b"." {
                continue;
            }
            let mut full = [0u8; MAX_PATH];
            let fl = join_into(path, nm, &mut full);
            if fl == 0 {
                continue;
            }
            let (t, sz) = stat_path(&full[..fl]);
            ENT_TYPE[i] = t;
            ENT_SIZE[i] = sz;
        }
        if SEL_IDX >= ENT_COUNT {
            SEL_IDX = usize::MAX;
        }
    }
}

// ==================== 复制 / 移动（子进程执行 + 进度文件） ====================

/// 目录递归总字节数（预统计，供进度条）。
unsafe fn dir_total(dir: &[u8], depth: usize) -> u64 {
    if depth > 10 {
        return 0;
    }
    let fd = open_path(dir, O_RDONLY);
    if fd < 0 {
        return 0;
    }
    let mut total = 0u64;
    let mut raw = [0u8; 2048];
    loop {
        let n = syscall3(SYS_GETDENTS64, fd as usize, raw.as_mut_ptr() as usize, raw.len());
        if n as isize <= 0 {
            break;
        }
        let mut off = 0usize;
        while off + 19 <= n as usize {
            let reclen = u16::from_le_bytes([raw[off + 16], raw[off + 17]]) as usize;
            if reclen < 19 || off + reclen > n as usize {
                break;
            }
            let dtyp = raw[off + 18];
            let name = &raw[off + 19..off + reclen];
            let elen = name.iter().position(|&c| c == 0).unwrap_or(name.len());
            if elen > 0 && name[0] != b'.' {
                let mut child = [0u8; MAX_PATH];
                let cl = join_into(dir, &name[..elen], &mut child);
                if cl > 0 {
                    if dtyp == DT_DIR {
                        total += dir_total(&child[..cl], depth + 1);
                    } else {
                        total += stat_path(&child[..cl]).1;
                    }
                }
            }
            off += reclen;
        }
    }
    syscall1(SYS_CLOSE, fd as usize);
    total
}

/// 写进度文件："<stage> <done> <total>"。
unsafe fn write_progress(stage: u8, done: u64, total: u64) {
    let fd = syscall3(
        SYS_OPENAT,
        AT_FDCWD,
        CP_STATE.as_ptr() as usize,
        2 | 0x40 | 0x200, // O_RDWR | O_CREAT | O_TRUNC
    ) as isize;
    if fd < 0 {
        return;
    }
    let mut line = [0u8; 48];
    let mut n = 0usize;
    line[n] = b'0' + stage;
    n += 1;
    line[n] = b' ';
    n += 1;
    n = push_u64(&mut line, n, done);
    line[n] = b' ';
    n += 1;
    n = push_u64(&mut line, n, total);
    line[n] = b'\n';
    n += 1;
    syscall3(SYS_WRITE, fd as usize, line.as_ptr() as usize, n);
    syscall1(SYS_CLOSE, fd as usize);
}

fn push_u64(buf: &mut [u8], mut n: usize, mut v: u64) -> usize {
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    if v == 0 {
        buf[n] = b'0';
        return n + 1;
    }
    while v > 0 {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    while i < tmp.len() {
        buf[n] = tmp[i];
        n += 1;
        i += 1;
    }
    n
}

/// 复制单个文件 src → dst。
unsafe fn copy_one(src: &[u8], dst: &[u8], counter: &mut u64, last_report: &mut u64) -> bool {
    let s = open_path(src, O_RDONLY);
    if s < 0 {
        return false;
    }
    let d = open_path(dst, 1 | 0x40 | 0x200); // O_WRONLY|O_CREAT|O_TRUNC
    if d < 0 {
        syscall1(SYS_CLOSE, s as usize);
        return false;
    }
    let mut ok = true;
    loop {
        let nr = syscall3(SYS_READ, s as usize, CP_BUF.as_mut_ptr() as usize, CP_BUF.len());
        if nr as isize <= 0 {
            break;
        }
        let nw = syscall3(SYS_WRITE, d as usize, CP_BUF.as_ptr() as usize, nr);
        if nw != nr {
            ok = false;
            break;
        }
        *counter += nr as u64;
        if counter.wrapping_sub(*last_report) >= 262144 {
            *last_report = *counter;
            write_progress(0, *counter, *counter);
        }
    }
    syscall1(SYS_CLOSE, s as usize);
    syscall1(SYS_CLOSE, d as usize);
    ok
}

/// 递归复制目录树 src_dir → dst_dir。
unsafe fn copy_tree(src_dir: &[u8], dst_dir: &[u8], depth: usize, counter: &mut u64, total: u64, last_report: &mut u64) -> bool {
    if depth > 10 {
        return false;
    }
    if mkdir_path(dst_dir) != 0 && stat_path(dst_dir).0 != DT_DIR {
        return false;
    }
    let fd = open_path(src_dir, O_RDONLY);
    if fd < 0 {
        return false;
    }
    let mut ok = true;
    let mut raw = [0u8; 2048];
    loop {
        if !ok {
            break;
        }
        let n = syscall3(SYS_GETDENTS64, fd as usize, raw.as_mut_ptr() as usize, raw.len());
        if n as isize <= 0 {
            break;
        }
        let mut off = 0usize;
        while off + 19 <= n as usize {
            let reclen = u16::from_le_bytes([raw[off + 16], raw[off + 17]]) as usize;
            if reclen < 19 || off + reclen > n as usize {
                break;
            }
            let dtyp = raw[off + 18];
            let name = &raw[off + 19..off + reclen];
            let elen = name.iter().position(|&c| c == 0).unwrap_or(name.len());
            if elen > 0 && name[0] != b'.' {
                let mut cs = [0u8; MAX_PATH];
                let mut cd = [0u8; MAX_PATH];
                let csl = join_into(src_dir, &name[..elen], &mut cs);
                let cdl = join_into(dst_dir, &name[..elen], &mut cd);
                if csl > 0 && cdl > 0 {
                    if dtyp == DT_DIR {
                        if !copy_tree(&cs[..csl], &cd[..cdl], depth + 1, counter, total, last_report) {
                            ok = false;
                            break;
                        }
                    } else if !copy_one(&cs[..csl], &cd[..cdl], counter, last_report) {
                        ok = false;
                        break;
                    }
                }
            }
            off += reclen;
            if counter.wrapping_sub(*last_report) >= 524288 {
                *last_report = *counter;
                write_progress(0, *counter, total);
            }
        }
    }
    syscall1(SYS_CLOSE, fd as usize);
    ok
}

/// 子进程：复制/移动 OP_SRC → OP_DST（OP_DST 为完整目标路径）。
fn child_run_copy() -> usize {
    unsafe {
        let src = &OP_SRC[..OP_SRC_LEN];
        let dst = &OP_DST[..OP_DST_LEN];
        let (ty, sz) = stat_path(src);
        if ty == DT_DIR {
            let total = dir_total(src, 0);
            write_progress(0, 0, total);
            let mut counter = 0u64;
            let mut last_report = 0u64;
            copy_tree(src, dst, 0, &mut counter, total, &mut last_report);
        } else {
            write_progress(0, 0, sz);
            let mut counter = 0u64;
            let mut last_report = 0u64;
            copy_one(src, dst, &mut counter, &mut last_report);
        }
        write_progress(1, 0, 0);
    }
    0
}

/// 子进程：移动（跨文件系统回退）= 复制后删除源。
fn child_run_move() -> usize {
    unsafe {
        let src = &OP_SRC[..OP_SRC_LEN];
        let dst = &OP_DST[..OP_DST_LEN];
        let (ty, sz) = stat_path(src);
        if ty == DT_DIR {
            write_progress(2, 0, 0);
            return 0;
        }
        write_progress(0, 0, sz);
        let mut counter = 0u64;
        let mut last_report = 0u64;
        if copy_one(src, dst, &mut counter, &mut last_report) {
            unlink_path(src);
            write_progress(1, 0, 0);
        } else {
            write_progress(2, 0, 0);
        }
    }
    0
}

fn fork_child(f: fn() -> usize) -> i32 {
    unsafe {
        let pid = syscall5(SYS_CLONE, SIGCHLD, 0, 0, 0, 0) as isize;
        if pid == 0 {
            let _ = f();
            syscall1(SYS_EXIT, 0);
            loop {}
        }
        pid as i32
    }
}

/// 读取进度文件，解析 (stage, done, total)。
fn read_progress() -> (u8, u64, u64) {
    unsafe {
        let fd = syscall3(
            SYS_OPENAT,
            AT_FDCWD,
            CP_STATE.as_ptr() as usize,
            0, // O_RDONLY
        ) as isize;
        if fd < 0 {
            return (0, 0, 0);
        }
        let mut buf = [0u8; 48];
        let n = syscall3(SYS_READ, fd as usize, buf.as_mut_ptr() as usize, buf.len()) as isize;
        syscall1(SYS_CLOSE, fd as usize);
        if n < 3 {
            return (0, 0, 0);
        }
        let mut stage = 0u8;
        let mut i = 0usize;
        if buf[0].is_ascii_digit() {
            stage = buf[0] - b'0';
        }
        while i < n as usize && buf[i] != b' ' {
            i += 1;
        }
        while i < n as usize && buf[i] == b' ' {
            i += 1;
        }
        let mut done = 0u64;
        while i < n as usize && buf[i].is_ascii_digit() {
            done = done * 10 + (buf[i] - b'0') as u64;
            i += 1;
        }
        while i < n as usize && buf[i] == b' ' {
            i += 1;
        }
        let mut total = 0u64;
        while i < n as usize && buf[i].is_ascii_digit() {
            total = total * 10 + (buf[i] - b'0') as u64;
            i += 1;
        }
        (stage, done, total)
    }
}

// ==================== 关联应用 ====================

/// 取扩展名（最后一个 '.' 之后，小写写进 out，NUL 结尾），返回元组 (out, len)。
fn ext_of(name: &[u8], out: &mut [u8; 8]) -> usize {
    let mut dot = 0usize;
    for (i, &c) in name.iter().enumerate() {
        if c == b'.' {
            dot = i + 1;
        }
    }
    let mut n = 0usize;
    for &c in &name[dot..] {
        if n + 1 >= out.len() {
            break;
        }
        out[n] = if c >= b'A' && c <= b'Z' { c + 32 } else { c };
        n += 1;
    }
    n
}

fn ext_eq(ext: &[u8], n: usize, want: &[u8]) -> bool {
    n == want.len() && &ext[..n] == want
}

/// 扩展名 → 关联应用；None = 内置文本查看器。
fn app_for(ext: &[u8], n: usize) -> Option<&'static [u8]> {
    if ext_eq(ext, n, b"wav")
        || ext_eq(ext, n, b"mp3")
        || ext_eq(ext, n, b"ogg")
        || ext_eq(ext, n, b"flac")
        || ext_eq(ext, n, b"aac")
        || ext_eq(ext, n, b"m4a")
    {
        Some(b"/bin/music")
    } else if ext_eq(ext, n, b"jpg")
        || ext_eq(ext, n, b"jpeg")
        || ext_eq(ext, n, b"png")
        || ext_eq(ext, n, b"gif")
        || ext_eq(ext, n, b"bmp")
        || ext_eq(ext, n, b"webp")
    {
        Some(b"/bin/gallery")
    } else {
        None
    }
}

/// 打开文件：文本进内置查看器；音/图启动关联应用（fork + execve，等待其退出）。
fn open_file(full: &[u8], name: &[u8]) {
    let mut ext = [0u8; 8];
    let n = ext_of(name, &mut ext);
    match app_for(&ext, n) {
        Some(exe) => {
            let pid = unsafe {
                static mut A0: [u8; 32] = [0; 32];
                static mut A1: [u8; MAX_PATH] = [0; MAX_PATH];
                static mut ARGV: [usize; 3] = [0; 3];
                let a0 = &mut *core::ptr::addr_of_mut!(A0);
                let a1 = &mut *core::ptr::addr_of_mut!(A1);
                let argv = &mut *core::ptr::addr_of_mut!(ARGV);
                let el = exe.len().min(a0.len() - 1);
                a0[..el].copy_from_slice(&exe[..el]);
                a0[el] = 0;
                let fl = full.len().min(a1.len() - 1);
                a1[..fl].copy_from_slice(&full[..fl]);
                a1[fl] = 0;
                argv[0] = a0.as_ptr() as usize;
                argv[1] = a1.as_ptr() as usize;
                argv[2] = 0;
                let pid = syscall5(SYS_CLONE, SIGCHLD, 0, 0, 0, 0) as i32;
                if pid == 0 {
                    syscall3(SYS_EXECVE, a0.as_ptr() as usize, argv.as_ptr() as usize, 0);
                    syscall1(SYS_EXIT, 127);
                    loop {}
                }
                pid
            };
            if pid > 0 {
                let mut status = 0i32;
                unsafe {
                    syscall4(SYS_WAIT4, pid as usize, &mut status as *mut i32 as usize, 0, 0);
                }
                print("[files] 应用已退出\n");
            }
            show_error(b"");
            load_dir(cwd_nul());
            redraw();
        }
        None => {
            open_viewer(full);
        }
    }
}

// ==================== 查看器 ====================

fn open_viewer(full: &[u8]) {
    unsafe {
        let n = full.len().min(VIEW_PATH.len() - 1);
        VIEW_PATH[..n].copy_from_slice(&full[..n]);
        VIEW_PATH[n] = 0;
        let fd = open_path(full, O_RDONLY);
        if fd < 0 {
            show_error("无法打开文件".as_bytes());
            return;
        }
        let mut total = 0usize;
        while total < VIEW_CAP {
            let r = syscall3(
                SYS_READ,
                fd as usize,
                VIEW_BUF.as_mut_ptr().add(total) as usize,
                VIEW_CAP - total,
            ) as isize;
            if r <= 0 {
                break;
            }
            total += r as usize;
        }
        syscall1(SYS_CLOSE, fd as usize);
        VIEW_LEN = total;
        VIEW_SCROLL = 0;
        snapshot_page();
        begin_transition(true);
        UI_MODE = MODE_VIEW;
        VIEW_PATH[n] = 0;
    }
}

/// 查看器行数统计。
fn viewer_lines() -> (usize, usize) {
    let mut lines = 1usize;
    unsafe {
        for i in 0..VIEW_LEN {
            if VIEW_BUF[i] == b'\n' {
                lines += 1;
            }
        }
    }
    let rows = ((NAV_Y - TITLE_H - 24) / 20) as usize;
    (lines, rows.max(1))
}

// ==================== 绘制 ====================

fn fill_screen(color: u32) {
    fill_rect(0, 0, DISPLAY_W as i32, DISPLAY_H as i32, color);
}

/// 尺寸格式化："512B" / "3KB" / "1.2MB" / "4.5GB"，返回长度。
fn fmt_size(v: u64, out: &mut [u8]) -> usize {
    let mut n = 0usize;
    let gb = 1024u64 * 1024 * 1024;
    let mb = 1024 * 1024;
    let kb = 1024;
    if v >= gb {
        let w = (v * 10 / gb) as u32;
        // 整数部分逐位写出：原实现假设只有一位数字 —— 512GB 卷的 w/10=512
        // 经 `as u8` 截断为 0（显示成「0.0GB」），≥100 单位还会产出非 ASCII
        // 字节喂给文本渲染器
        n = push_dec_u32(out, n, w / 10);
        out[n] = b'.';
        n += 1;
        out[n] = b'0' + (w % 10) as u8;
        n += 1;
        out[n] = b'G';
        out[n + 1] = b'B';
        n += 2;
    } else if v >= mb {
        let w = (v * 10 / mb) as u32;
        n = push_dec_u32(out, n, w / 10);
        out[n] = b'.';
        n += 1;
        out[n] = b'0' + (w % 10) as u8;
        n += 1;
        out[n] = b'M';
        out[n + 1] = b'B';
        n += 2;
    } else if v >= kb {
        let w = (v / kb) as u32;
        n = push_dec_u32(out, n, w);
        out[n] = b'K';
        out[n + 1] = b'B';
        n += 2;
    } else {
        n = push_dec_u32(out, n, v as u32);
        out[n] = b'B';
        n += 1;
    }
    n
}

fn push_dec_u32(out: &mut [u8], mut n: usize, mut v: u32) -> usize {
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
    while i < tmp.len() {
        out[n] = tmp[i];
        n += 1;
        i += 1;
    }
    n
}

/// 文件夹图标（小）。
fn draw_folder_icon(x: i32, y: i32, color: u32) {
    fill_rect(x, y + 1, 6, 3, color);
    fill_rect(x + 1, y + 3, 18, 13, color);
    fill_rect(x + 3, y + 4, 14, 10, C_BG);
}

/// 文件图标（小）。
fn draw_file_icon(x: i32, y: i32, color: u32) {
    fill_rect(x + 3, y, 12, 16, color);
    fill_rect(x + 6, y + 3, 6, 1, C_BG);
    fill_rect(x + 6, y + 7, 6, 1, C_BG);
    fill_rect(x + 6, y + 11, 6, 1, C_BG);
}

/// 视图：图标 + 选中圈。
fn draw_list() {
    unsafe {
        let rows_visible = ((LIST_BOTTOM - LIST_TOP) / ROW_H) as usize;
        let max_scroll = ENT_COUNT.saturating_sub(rows_visible);
        if LIST_SCROLL > max_scroll {
            LIST_SCROLL = max_scroll;
        }
        for r in 0..rows_visible {
            let idx = LIST_SCROLL + r;
            if idx >= ENT_COUNT {
                break;
            }
            let y = LIST_TOP + r as i32 * ROW_H;
            draw_glass(8, y + 3, DISPLAY_W as i32 - 16, ROW_H - 6, 12);
            let nm = &ENT_NAMES[idx][..ENT_LEN[idx]];
            let t = ENT_TYPE[idx];
            // 图标 + 名称颜色。
            let (icon_color, name_color) = match t {
                DT_DIR => (C_FOLDER, if idx == SEL_IDX { C_SEL } else { C_TEXT }),
                DT_LNK => (C_LINK, if idx == SEL_IDX { C_SEL } else { C_TEXT }),
                2 | 6 => (C_SEL, C_TEXT),
                _ => (C_FILE, if idx == SEL_IDX { C_SEL } else { C_TEXT }),
            };
            if idx == SEL_IDX {
                // 选中：左侧高亮条。
                fill_rect(8, y + 6, 4, ROW_H - 12, C_ACCENT);
            }
            if t == DT_DIR {
                draw_folder_icon(20, y + (ROW_H - 16) / 2, icon_color);
            } else {
                draw_file_icon(20, y + (ROW_H - 16) / 2, icon_color);
            }
            // 名称。
            let namew = DISPLAY_W as i32 - 24 - 96;
            let mut w = 0i32;
            let mut cut = ENT_LEN[idx];
            for i in (0..ENT_LEN[idx]).rev() {
                let (cp, n2) = ttf_text::font_render::utf8_next(nm, i);
                let cw = if cp >= 0x80 { 16 } else { 8 };
                if w + cw > namew {
                    cut = i + n2;
                    break;
                }
                w += cw;
            }
            draw_text(48, y + (ROW_H - 8) / 2, &nm[..cut], name_color);
            if t == DT_LNK {
                draw_text(48 + w, y + (ROW_H - 8) / 2, b" ->", C_LINK);
            }
            // 大小。
            if t != DT_DIR && t != DT_LNK && t != 2 && t != 6 {
                let mut szb = [0u8; 16];
                let sl = fmt_size(ENT_SIZE[idx], &mut szb);
                draw_text(
                    DISPLAY_W as i32 - 12 - 54 - text_width(&szb[..sl]),
                    y + (ROW_H - 8) / 2,
                    &szb[..sl],
                    C_DIM,
                );
            }
            // 选中圈（右侧）。
            let cx = DISPLAY_W as i32 - 34;
            let cy = y + ROW_H / 2 - 8;
            draw_circle(cx, cy, 8, if idx == SEL_IDX { C_ACCENT } else { C_DIM });
            if idx == SEL_IDX {
                draw_disc(cx, cy, 6, C_ACCENT);
                // 强调绿圆盘上的选中勾保持白色。
                fill_rect(cx - 3, cy, 2, 6, C_ON_ACCENT);
                fill_rect(cx, cy + 1, 2, 4, C_ON_ACCENT);
            }
        }
    }
}

/// 路径栏：分段 chip，点击跳转。
fn render_path_bar() {
    unsafe {
        let mut plen = cwd_len();
        if plen == 0 {
            plen = 1;
        }
        let path = &CWD[..plen];
        // 收集段起点。
        let mut offs = [0usize; 8];
        offs[0] = 0; // "/"（根）
        let mut seg = 1usize;
        for i in 1..plen {
            if path[i] == b'/' && i + 1 < plen {
                offs[seg] = i + 1;
                seg += 1;
                if seg >= 8 {
                    break;
                }
            }
        }
        let seg_n = seg;
        let mut x = 10i32;
        // 段过多时：显示根 + 末尾 6 段。
        let first = if seg_n > 7 { seg_n - 6 - 1 } else { 0 };
        let mut seg_i = first;
        let mut xi = 0usize;
        while seg_i < seg_n {
            let start = offs[seg_i];
            let end = if seg_i + 1 < seg_n {
                let mut e = offs[seg_i + 1];
                while path[e - 1] == b'/' {
                    e -= 1;
                }
                e
            } else {
                plen
            };
            let comp = &path[start..end];
            let label: &[u8] = if seg_i == 0 {
                b"/"
            } else if first > 0 && seg_i == first {
                b".."
            } else {
                comp
            };
            let w = text_width(label) + 14;
            let mut tw = if seg_i == 0 {
                w.max(26)
            } else {
                w
            };
            if x + tw > DISPLAY_W as i32 - 10 {
                tw = (DISPLAY_W as i32 - 10 - x).max(16);
                if tw < w {
                    // 截断：省略尾部。
                    let _ = tw;
                    SEG_X[xi] = x as usize;
                    SEG_W[xi] = 16;
                    SEG_OFF[xi] = start;
                    draw_glass(x, PATH_BAR_Y + 2, 16, PATH_BAR_H - 4, 8);
                    draw_text(x, PATH_BAR_Y + 6, b"..", C_DIM);
                    xi += 1;
                    break;
                }
            }
            let is_cur = seg_i == seg_n - 1;
            if is_cur {
                draw_glass_style(x, PATH_BAR_Y + 2, tw, PATH_BAR_H - 4, 8, &STYLE_ACCENT);
            } else {
                draw_glass(x, PATH_BAR_Y + 2, tw, PATH_BAR_H - 4, 8);
            }
            draw_text(
                x + 7,
                PATH_BAR_Y + 6,
                label,
                if is_cur { C_ON_ACCENT } else { C_DIM },
            );
            SEG_X[xi] = x as usize;
            SEG_W[xi] = tw as usize;
            SEG_OFF[xi] = start;
            xi += 1;
            x += tw + 10;
            seg_i += 1;
        }
        SEG_N = xi;
        // 右端提示：当前路径太长时省略。
        if x >= DISPLAY_W as i32 - 40 {
            // 已截断处理。
        }
    }
}

/// 路径栏命中：返回要跳转到的目录（NUL 结尾于 OP_DST），None 无命中。
fn path_bar_hit(tx: i32) -> Option<usize> {
    unsafe {
        let mut target = [0u8; 128];
        for i in 0..SEG_N {
            let sx = SEG_X[i] as i32;
            let sw = SEG_W[i] as i32;
            if tx >= sx && tx < sx + sw {
                let off = SEG_OFF[i];
                // 跳转到 CWD[..off]（不含末尾 /）。
                let mut n = 0usize;
                for &c in CWD.iter().take(off) {
                    target[n] = c;
                    n += 1;
                    if n + 1 >= target.len() {
                        break;
                    }
                }
                while n > 1 && target[n - 1] == b'/' {
                    n -= 1;
                }
                if n == 0 {
                    target[0] = b'/';
                    n = 1;
                }
                target[n] = 0;
                CWD.fill(0);
                CWD[..n].copy_from_slice(&target[..n]);
                return Some(n);
            }
        }
    }
    None
}

/// 存储条：总 / 已用 / 剩余 + 使用率条。
fn render_storage() {
    let (total, free) = statfs_bytes(cwd_nul());
    let used = total.saturating_sub(free);
    // 96 字节：固定文案 26B + 三段 fmt_size（每段最长 "1023.9MB" 8B），
    // 48B 在 GB 级卷（如 1GB 盘 used/free 各 "1023.9MB"）会越界 panic。
    let mut line = [0u8; 96];
    let mut n = 0usize;
    for &c in "存储 ".as_bytes() {
        line[n] = c;
        n += 1;
    }
    n = push_fmt(&mut line, n, used);
    line[n] = b' ';
    n += 1;
    line[n] = b'/';
    n += 1;
    line[n] = b' ';
    n += 1;
    n = push_fmt(&mut line, n, total);
    line[n] = b' ';
    n += 1;
    for &c in "已用".as_bytes() {
        line[n] = c;
        n += 1;
    }
    for &c in " · 剩".as_bytes() {
        line[n] = c;
        n += 1;
    }
    for &c in "余 ".as_bytes() {
        line[n] = c;
        n += 1;
    }
    n = push_fmt(&mut line, n, free);
    draw_text(12, STORE_Y, &line[..n], C_DIM);
    // 使用率条。
    let bw = DISPLAY_W as i32 - 24 - 8 * text_width("剩余 ".as_bytes()) - 100;
    let bw = bw.max(120);
    let bar_x = DISPLAY_W as i32 - 12 - bw;
    let frac = if total > 0 {
        ((used as u64 * bw as u64 / total as u64) as i32).min(bw)
    } else {
        0
    };
    draw_glass(bar_x, STORE_Y + 6, bw, 10, 5);
    if frac > 0 {
        fill_rect(bar_x + 2, STORE_Y + 8, frac - 2, 6, C_ACCENT);
    }
}

fn push_fmt(out: &mut [u8], n: usize, v: u64) -> usize {
    let mut tmp = [0u8; 16];
    let l = fmt_size(v, &mut tmp);
    // 有界追加：n 越界时截断而不是越界写（fmt_size 输出最长 14B）。
    if n >= out.len() {
        return n;
    }
    let l = l.min(out.len() - n);
    out[n..n + l].copy_from_slice(&tmp[..l]);
    n + l
}

/// 底部工具栏（5 个按钮；剪贴板挂起时前两格变 粘贴/取消）。
fn render_toolbar() {
    unsafe {
        let pending = CLIP_OP != CLIP_NONE;
        let labels: [&[u8]; 5] = if pending {
            ["粘贴".as_bytes(), "取消".as_bytes(), "新建".as_bytes(), "复制".as_bytes(), "移动".as_bytes()]
        } else {
            ["新建".as_bytes(), "复制".as_bytes(), "移动".as_bytes(), "重命名".as_bytes(), "删除".as_bytes()]
        };
        let gap = 8i32;
        let n = 5i32;
        let sw = (DISPLAY_W as i32 - 16 - gap * (n - 1)) / n;
        for i in 0..5i32 {
            let x = 8 + i * (sw + gap);
            let accent = (pending && i == 0) || (!pending && i == 0);
            let danger = !pending && i == 4;
            if danger {
                draw_glass_style(x, TOOL_Y, sw, TOOL_H - 6, 12, &STYLE_DANGER);
            } else if accent {
                draw_glass_style(x, TOOL_Y, sw, TOOL_H - 6, 12, &STYLE_ACCENT);
            } else {
                draw_glass(x, TOOL_Y, sw, TOOL_H - 6, 12);
            }
            let w = text_width(labels[i as usize]);
            // 彩色玻璃（强调绿/危险红）上保持白字，浅玻璃上用深色文字。
            let tc = if danger || accent { C_ON_ACCENT } else { C_TEXT };
            draw_text(x + (sw - w) / 2, TOOL_Y + (TOOL_H - 6 - 20) / 2, labels[i as usize], tc);
        }
        // 剪贴板状态提示。
        if pending {
            let mut hint = [0u8; 96];
            let mut n2 = 0usize;
            for &c in (if CLIP_OP == CLIP_COPY { "已复制：" } else { "已剪切：" }).as_bytes() {
                hint[n2] = c;
                n2 += 1;
            }
            for &c in &CLIP_NAME[..CLIP_NAME_LEN] {
                hint[n2] = c;
                n2 += 1;
            }
            for &c in " → 去目标目录点粘贴".as_bytes() {
                hint[n2] = c;
                n2 += 1;
            }
            draw_text(12, TOOL_Y - 20, &hint[..n2], C_SEL);
        } else {
            draw_text(12, TOOL_Y - 20, "点右侧圆点选择项目".as_bytes(), C_DIM);
        }
    }
}

fn draw_browse() {
    let dark = glass_dark();
    draw_gradient_bg(C_BG, dark);
    draw_title_bar("文件管理器".as_bytes());
    render_path_bar();
    render_storage();
    draw_list();
    render_toolbar();
    draw_sys_nav();
}

fn draw_viewer() {
    unsafe {
        let dark = glass_dark();
        draw_gradient_bg(C_BG, dark);
        draw_glass(0, 0, DISPLAY_W as i32, TITLE_H, 0);
        draw_glass(4, 4, 36, TITLE_H - 8, 10);
        draw_text(12, 10, b"<", C_TEXT);
        draw_text(48, 10, "查看文件".as_bytes(), C_TEXT);
        let (total_lines, rows) = viewer_lines();
        let max_scroll = total_lines.saturating_sub(rows);
        if VIEW_SCROLL > max_scroll {
            VIEW_SCROLL = max_scroll;
        }
        let mut line = 0usize;
        let mut start = 0usize;
        let mut y = TITLE_H + 6;
        let first = VIEW_SCROLL;
        let last = (VIEW_SCROLL + rows).min(total_lines);
        let mut idx = 0usize;
        while idx <= VIEW_LEN {
            if idx == VIEW_LEN || VIEW_BUF[idx] == b'\n' {
                if line >= first && line < last {
                    let mut seg = [0u8; 512];
                    let mut k = 0usize;
                    let mut kk = start;
                    while kk < idx && k + 1 < seg.len() {
                        let c = VIEW_BUF[kk];
                        if c != b'\r' {
                            seg[k] = c;
                            k += 1;
                        }
                        kk += 1;
                    }
                    draw_text(8, y, &seg[..k], C_TEXT);
                    y += 20;
                }
                line += 1;
                start = idx + 1;
                idx += 1;
            } else {
                idx += 1;
            }
        }
        // 底部状态。
        let mut vp = [0u8; MAX_PATH];
        let mut vl = 0usize;
        while vl < VIEW_PATH.len() && VIEW_PATH[vl] != 0 {
            vp[vl] = VIEW_PATH[vl];
            vl += 1;
        }
        draw_text(8, NAV_Y - 18, &vp[..vl], C_DIM);
        let mut st = [0u8; 32];
        let mut off = 0usize;
        for &c in "第 ".as_bytes() {
            st[off] = c;
            off += 1;
        }
        off = push_dec_u32(&mut st, off, (VIEW_SCROLL + 1) as u32);
        for &c in "/共".as_bytes() {
            st[off] = c;
            off += 1;
        }
        off = push_dec_u32(&mut st, off, total_lines as u32);
        for &c in " 行".as_bytes() {
            st[off] = c;
            off += 1;
        }
        draw_text(DISPLAY_W as i32 - 8 - text_width(&st[..off]), NAV_Y - 18, &st[..off], C_DIM);
        draw_sys_nav();
    }
}

// ==================== 对话框（shared/dialog OriginOS 风格） ====================

/// 对话框几何高度（绘制与命中测试共用，永不漂移）。
const DLG_INPUT_H: i32 =
    dialog::DLG_TITLE_H + dialog::DLG_FIELD_ROW_H + dialog::DLG_MSG_H + dialog::DLG_BTN_H;
const DLG_CONFIRM_H: i32 =
    dialog::DLG_TITLE_H + dialog::DLG_MSG_H * 2 + dialog::DLG_BTN_H + 8;
const DLG_PROGRESS_H: i32 =
    dialog::DLG_TITLE_H + dialog::DLG_MSG_H * 2 + dialog::DLG_PROG_ROW_H + dialog::DLG_BTN_H;

/// app_kit 管线适配（像素写 BACK_BUF，文本走 TTF 固定字号）。
struct DlgUi;

impl dialog::DialogUi for DlgUi {
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
    fn text(&mut self, x: i32, y: i32, s: &[u8], _px: f32, color: u32) {
        draw_text(x, y, s, color);
    }
    fn text_bold(&mut self, x: i32, y: i32, s: &[u8], _px: f32, color: u32) {
        draw_text_bold(x, y, s, color);
    }
    fn text_w(&self, s: &[u8], _px: f32) -> i32 {
        text_width(s)
    }
    fn text_top(&self, _px: f32, cy: i32) -> i32 {
        // app_kit 文本以字形顶部为 y；16px 字形垂直居中的经验偏移（与
        // 旧 draw_center_y 的 (40-20)/2 一致）。
        cy - 10
    }
    fn fill_row(&mut self, y: i32, x0: i32, x1: i32, color: u32, alpha: u32) {
        if y < 0 || y >= DISPLAY_H as i32 || x1 <= x0 {
            return;
        }
        let xa = x0.max(0);
        let xb = x1.min(DISPLAY_W as i32);
        if xb <= xa {
            return;
        }
        unsafe {
            let base = y as usize * DISPLAY_W;
            for idx in base + xa as usize..base + xb as usize {
                BACK_BUF[idx] = dialog::blend_into(BACK_BUF[idx], color, alpha);
            }
        }
    }
}

/// glass 抗锯齿圆角矩形适配（shared/dialog RrFn 签名）。
fn dlg_fill(
    _ui: &mut dyn dialog::DialogUi,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    r: i32,
    c: u32,
    _a: u32,
) {
    aa_fill_round_rect(x, y, w, h, r, c);
}

fn dlg_stroke(
    _ui: &mut dyn dialog::DialogUi,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    r: i32,
    c: u32,
    a: u32,
) {
    aa_stroke_round_rect(x, y, w, h, r, c, a);
}

fn draw_input_dialog() {
    unsafe {
        let sw = DISPLAY_W as i32;
        let sh = DISPLAY_H as i32;
        let w = dialog::DLG_W;
        let h = DLG_INPUT_H;
        let (x, y) = dialog::center_rect(sw, sh, w, h);
        let mut ui = DlgUi;
        dialog::scrim(&mut ui, sw, sh, 115);
        dialog::draw_card(&mut ui, dlg_fill, dlg_stroke, x, y, w, h);
        let title: &[u8] = if INPUT_ACTION == ACT_NEW_FOLDER {
            "新建文件夹".as_bytes()
        } else {
            "重命名".as_bytes()
        };
        dialog::draw_title(&mut ui, x, y, w, title);
        dialog::draw_input_field(
            &mut ui,
            dlg_fill,
            x + 24,
            y + dialog::DLG_TITLE_H + 8,
            w - 48,
            &INPUT_BUF[..INPUT_LEN],
        );
        dialog::draw_message(
            &mut ui,
            x,
            y + dialog::DLG_TITLE_H + dialog::DLG_FIELD_ROW_H,
            w,
            "使用键盘输入名称".as_bytes(),
            14.0,
        );
        dialog::draw_buttons(
            &mut ui,
            x,
            y + h - dialog::DLG_BTN_H,
            w,
            "取消".as_bytes(),
            Some(("确定".as_bytes(), false)),
        );
    }
}

fn draw_confirm_dialog() {
    unsafe {
        let sw = DISPLAY_W as i32;
        let sh = DISPLAY_H as i32;
        let w = dialog::DLG_W;
        let h = DLG_CONFIRM_H;
        let (x, y) = dialog::center_rect(sw, sh, w, h);
        let mut ui = DlgUi;
        dialog::scrim(&mut ui, sw, sh, 115);
        dialog::draw_card(&mut ui, dlg_fill, dlg_stroke, x, y, w, h);
        dialog::draw_title(&mut ui, x, y, w, "确认删除".as_bytes());
        // 将删除：<短名>。
        let plen = nul_len(&CONFIRM_PATH);
        let name_start = CONFIRM_PATH[..plen]
            .iter()
            .rposition(|&c| c == b'/')
            .map(|i| i + 1)
            .unwrap_or(0);
        let mut msg = [0u8; 96];
        let mut n = 0usize;
        for &c in "将删除：".as_bytes() {
            msg[n] = c;
            n += 1;
        }
        let nm = &CONFIRM_PATH[name_start..plen];
        for &c in nm.iter().take(24) {
            msg[n] = c;
            n += 1;
        }
        dialog::draw_message(&mut ui, x, y + dialog::DLG_TITLE_H, w, &msg[..n], 15.0);
        dialog::draw_message(
            &mut ui,
            x,
            y + dialog::DLG_TITLE_H + dialog::DLG_MSG_H,
            w,
            "此操作不可恢复".as_bytes(),
            14.0,
        );
        dialog::draw_buttons(
            &mut ui,
            x,
            y + h - dialog::DLG_BTN_H,
            w,
            "取消".as_bytes(),
            Some(("删除".as_bytes(), true)),
        );
    }
}

fn draw_progress_dialog() {
    unsafe {
        let sw = DISPLAY_W as i32;
        let sh = DISPLAY_H as i32;
        let w = dialog::DLG_W;
        let h = DLG_PROGRESS_H;
        let (x, y) = dialog::center_rect(sw, sh, w, h);
        let mut ui = DlgUi;
        dialog::scrim(&mut ui, sw, sh, 115);
        dialog::draw_card(&mut ui, dlg_fill, dlg_stroke, x, y, w, h);
        let (stage, done, total) = read_progress();
        let label: &[u8] = match PROG_KIND {
            PROG_MOVE => "正在移动".as_bytes(),
            _ => "正在复制".as_bytes(),
        };
        dialog::draw_title(&mut ui, x, y, w, label);
        // 文件名。
        let mut nm = [0u8; 48];
        let mut n = 0usize;
        for &c in OP_NAME[..OP_NAME_LEN].iter() {
            nm[n] = c;
            n += 1;
        }
        dialog::draw_message(&mut ui, x, y + dialog::DLG_TITLE_H, w, &nm[..n], 14.0);
        // 进度条 + 百分比（失败 = 红 0% + 红字提示由下方 pct 行承载）。
        let frac = if total > 0 && stage == 0 {
            ((done * 100 / total) as u32).min(100)
        } else if stage == 0 {
            0
        } else {
            100
        };
        let bar_y = y + dialog::DLG_TITLE_H + dialog::DLG_MSG_H + 14;
        dialog::draw_progress(&mut ui, dlg_fill, x + 32, bar_y, w - 64, if stage == 2 { 0 } else { frac });
        // 百分比 / 失败提示行。
        let mut pct = [0u8; 24];
        let mut pn = 0usize;
        if stage == 2 {
            for &c in "操作失败".as_bytes() {
                pct[pn] = c;
                pn += 1;
            }
        } else {
            pn = push_dec_u32(&mut pct, pn, frac);
            pct[pn] = b'%';
            pn += 1;
        }
        dialog::draw_message(
            &mut ui,
            x,
            y + dialog::DLG_TITLE_H + dialog::DLG_MSG_H + dialog::DLG_PROG_ROW_H,
            w,
            &pct[..pn],
            14.0,
        );
        dialog::draw_buttons(
            &mut ui,
            x,
            y + h - dialog::DLG_BTN_H,
            w,
            "取消".as_bytes(),
            None,
        );
    }
}

fn render() {
    unsafe {
        match UI_MODE {
            MODE_VIEW => draw_viewer(),
            MODE_INPUT => {
                draw_browse();
                draw_input_dialog();
            }
            MODE_CONFIRM => {
                draw_browse();
                draw_confirm_dialog();
            }
            MODE_PROGRESS => {
                draw_browse();
                draw_progress_dialog();
            }
            _ => draw_browse(),
        }
        // 页面转场合成（目标页已画进 BACK_BUF，叠加滑动 + 快照补底）。
        if TRANS_T0 != 0 {
            let now = kit::now_us();
            let p = motion::progress(TRANS_T0, TRANS_DUR, now);
            compose_transition(Easing::FastOutSlowIn.eval(p));
            if p >= 1.0 {
                TRANS_T0 = 0;
            }
        }
    }
}

fn redraw() {
    render();
    flip();
    cursor_overlay_redraw();
}

// ==================== 导航 ====================

fn enter_dir(name: &[u8]) {
    let mut nb = [0u8; 128];
    unsafe {
        let cwd = &CWD[..nul_len(&CWD)];
        if join_into(cwd, name, &mut nb) > 0 && nb[0] != 0 {
            snapshot_page();
            begin_transition(true);
            CWD.fill(0);
            let mut n = 0usize;
            while n < nb.len() && nb[n] != 0 {
                CWD[n] = nb[n];
                n += 1;
            }
        }
    }
    load_dir(cwd_nul());
}

fn go_up() {
    unsafe {
        snapshot_page();
        begin_transition(false);
        let mut plen = nul_len(&CWD);
        while plen > 1 && CWD[plen - 1] == b'/' {
            plen -= 1;
        }
        let mut slash = plen;
        while slash > 1 && CWD[slash - 1] != b'/' {
            slash -= 1;
        }
        if slash <= 1 {
            CWD.fill(0);
            CWD[0] = b'/';
        } else {
            CWD[slash - 1] = 0;
        }
    }
    load_dir(cwd_nul());
}

/// 项全路径写入 OP_SRC，返回长度。
fn sel_full_path(out: &mut [u8; MAX_PATH]) -> usize {
    unsafe {
        let idx = SEL_IDX;
        let nm = &ENT_NAMES[idx][..ENT_LEN[idx]];
        join_into(&CWD[..nul_len(&CWD)], nm, out)
    }
}

// ==================== 操作 ====================

/// 输入对话框确认动作。
fn do_input_action() {
    unsafe {
        let name_len = INPUT_LEN;
        if name_len == 0 {
            show_error("名称不能为空".as_bytes());
            return;
        }
        match INPUT_ACTION {
            ACT_NEW_FOLDER => {
                let mut full = [0u8; MAX_PATH];
                let fl = join_into(&CWD[..nul_len(&CWD)], &INPUT_BUF[..name_len], &mut full);
                if fl == 0 {
                    show_error("路径过长".as_bytes());
                    return;
                }
                if mkdir_path(&full[..fl]) != 0 {
                    show_error("无法创建（已存在或无权限）".as_bytes());
                }
            }
            ACT_RENAME => {
                let old = &INPUT_TARG[..INPUT_TARG_LEN];
                let old_plen = nul_len(old);
                let old_name_start = old[..old_plen]
                    .iter()
                    .rposition(|&c| c == b'/')
                    .map(|i| i + 1)
                    .unwrap_or(0);
                let mut new_full = [0u8; MAX_PATH];
                let nfl = join_into(&old[..old_name_start], &INPUT_BUF[..name_len], &mut new_full);
                if nfl == 0 {
                    show_error("路径过长".as_bytes());
                    return;
                }
                if rename_path(&old[..old_plen], &new_full[..nfl]) != 0 {
                    show_error("重命名失败（同名或受限）".as_bytes());
                }
            }
            _ => {}
        }
        UI_MODE = MODE_BROWSE;
    }
    load_dir(cwd_nul());
}

/// 删除确认执行。
fn do_delete() {
    unsafe {
        let path = &CONFIRM_PATH[..CONFIRM_PATH_LEN];
        let (ty, _) = stat_path(path);
        if ty == DT_DIR {
            // 递归删除子项后删除目录（SFS 不支持删目录 → 报错）。
            let ok = del_tree(path);
            if !ok {
                show_error("无法删除文件夹（系统限制）".as_bytes());
            }
        } else {
            if unlink_path(path) != 0 {
                show_error("无法删除（文件被占用）".as_bytes());
            }
        }
        UI_MODE = MODE_BROWSE;
    }
    load_dir(cwd_nul());
}

/// 递归删除：文件直接删；目录先递归删子项再删自己。
fn del_tree(path: &[u8]) -> bool {
    del_tree_depth(path, 0)
}

/// 深度上限与 dir_total/copy_tree 对齐（每层栈帧含 raw[2048] ≈ 2.3KB，
/// 无上限的深目录链会打穿 1MB 用户栈）。
fn del_tree_depth(path: &[u8], depth: usize) -> bool {
    unsafe {
        if depth > 10 {
            return false;
        }
        let fd = open_path(path, O_RDONLY);
        if fd < 0 {
            return unlink_path(path) == 0;
        }
        let mut raw = [0u8; 2048];
        loop {
            let n = syscall3(SYS_GETDENTS64, fd as usize, raw.as_mut_ptr() as usize, raw.len());
            if n as isize <= 0 {
                break;
            }
            let mut off = 0usize;
            while off + 19 <= n as usize {
                let reclen = u16::from_le_bytes([raw[off + 16], raw[off + 17]]) as usize;
                if reclen < 19 || off + reclen > n as usize {
                    break;
                }
                let name = &raw[off + 19..off + reclen];
                let elen = name.iter().position(|&c| c == 0).unwrap_or(name.len());
                if elen > 0 && name[0] != b'.' {
                    let mut child = [0u8; MAX_PATH];
                    let cl = join_into(path, &name[..elen], &mut child);
                    if cl > 0 {
                        del_tree_depth(&child[..cl], depth + 1);
                    }
                }
                off += reclen;
            }
        }
        syscall1(SYS_CLOSE, fd as usize);
        unlink_path(path) == 0
    }
}

/// 复制/移动剪贴板 → 当前目录。
fn do_paste() {
    unsafe {
        if CLIP_OP == CLIP_NONE {
            return;
        }
        let src = &CLIP_PATH[..CLIP_LEN];
        let src_plen = nul_len(src);
        let src_parent_end = src[..src_plen]
            .iter()
            .rposition(|&c| c == b'/')
            .map(|i| i)
            .unwrap_or(0);
        let src_name = &src[src_parent_end + 1..src_plen];
        let cwd = &CWD[..nul_len(&CWD)];
        let same_dir = cwd == &src[..src_parent_end]
            || (cwd == b"/" && src_parent_end == 0);

        if CLIP_OP == CLIP_MOVE {
            if same_dir {
                show_error("目标与来源相同".as_bytes());
                return;
            }
            // 先试 rename（同文件系统内任意位置移动）。
            let mut dest = [0u8; MAX_PATH];
            let dl = join_into(cwd, src_name, &mut dest);
            if dl == 0 {
                show_error("路径过长".as_bytes());
                return;
            }
            if rename_path(&src[..src_plen], &dest[..dl]) == 0 {
                CLIP_OP = CLIP_NONE;
                show_error(b"");
                return;
            }
            // 跨文件系统：仅文件可复制后删除。
            let (ty, _) = stat_path(&src[..src_plen]);
            if ty == DT_DIR {
                show_error("无法跨区移动文件夹".as_bytes());
                return;
            }
            spawn_bg(PROG_MOVE, &src[..src_plen], &dest[..dl], src_name);
        } else {
            // 复制：目的名自动去重。
            let mut dest_name = [0u8; MAX_NAME];
            let mut dn = 0usize;
            for &c in src_name.iter() {
                if dn + 1 >= MAX_NAME {
                    break;
                }
                dest_name[dn] = c;
                dn += 1;
            }
            // 去重。
            let mut cand = [0u8; MAX_NAME];
            let mut cn = 0usize;
            for &c in dest_name[..dn].iter() {
                cand[cn] = c;
                cn += 1;
            }
            let stem_end = cand[..cn]
                .iter()
                .rposition(|&c| c == b'.')
                .unwrap_or(cn);
            let mut attempt = 0usize;
            let mut dnm = [0u8; MAX_NAME];
            let mut dnl;
            loop {
                dnm.fill(0);
                dnl = 0usize;
                for &c in cand[..stem_end].iter() {
                    dnm[dnl] = c;
                    dnl += 1;
                }
                for &c in "_copy".as_bytes() {
                    if dnl + 1 < MAX_NAME {
                        dnm[dnl] = c;
                        dnl += 1;
                    }
                }
                if attempt > 0 {
                    let mut tmp = [0u8; 12];
                    let mut ti = tmp.len();
                    let mut v = (attempt + 1) as u32;
                    while v > 0 {
                        ti -= 1;
                        tmp[ti] = b'0' + (v % 10) as u8;
                        v /= 10;
                    }
                    while ti < tmp.len() {
                        if dnl + 1 < MAX_NAME {
                            dnm[dnl] = tmp[ti];
                            dnl += 1;
                        }
                        ti += 1;
                    }
                }
                for &c in cand[stem_end..cn].iter() {
                    if dnl + 1 < MAX_NAME {
                        dnm[dnl] = c;
                        dnl += 1;
                    }
                }
                let mut probe = [0u8; MAX_PATH];
                if join_into(cwd, &dnm[..dnl], &mut probe) > 0 && stat_path(&probe[..nul_len(&probe)]).0 == 0 {
                    break;
                }
                attempt += 1;
                if attempt > 64 {
                    show_error("无法生成唯一名称".as_bytes());
                    return;
                }
            }
            let mut dest = [0u8; MAX_PATH];
            let dl = join_into(cwd, &dnm[..dnl], &mut dest);
            if dl == 0 {
                show_error("路径过长".as_bytes());
                return;
            }
            let (ty, sz) = stat_path(&src[..src_plen]);
            // 小文件（≤ 64KB）同步复制；目录或大文件走后端子进程。
            if ty != DT_DIR && sz <= 65536 {
                let mut counter = 0u64;
                let mut last_report = 0u64;
                if !copy_one(&src[..src_plen], &dest[..dl], &mut counter, &mut last_report) {
                    show_error("复制失败".as_bytes());
                }
                CLIP_OP = CLIP_NONE;
                show_error(b"");
                load_dir(cwd_nul());
                redraw();
                return;
            }
            spawn_bg(PROG_COPY, &src[..src_plen], &dest[..dl], src_name);
        }
    }
}

/// 启动后台子进程执行复制/移动，进入进度模式。
fn spawn_bg(kind: u8, src: &[u8], dst: &[u8], name: &[u8]) {
    unsafe {
        OP_SRC.fill(0);
        OP_DST.fill(0);
        OP_NAME.fill(0);
        OP_SRC_LEN = src.len().min(MAX_PATH - 1);
        OP_SRC[..OP_SRC_LEN].copy_from_slice(src);
        OP_SRC[OP_SRC_LEN] = 0;
        OP_DST_LEN = dst.len().min(MAX_PATH - 1);
        OP_DST[..OP_DST_LEN].copy_from_slice(dst);
        OP_DST[OP_DST_LEN] = 0;
        OP_NAME_LEN = name.len().min(MAX_NAME);
        OP_NAME[..OP_NAME_LEN].copy_from_slice(&name[..OP_NAME_LEN]);
        PROG_KIND = kind;
        let run: fn() -> usize = if kind == PROG_MOVE {
            child_run_move
        } else {
            child_run_copy
        };
        let pid = fork_child(run);
        if pid <= 0 {
            show_error("无法启动后台任务".as_bytes());
            return;
        }
        PROG_PID = pid;
        PROG_START_US = now_us();
        UI_MODE = MODE_PROGRESS;
        redraw();
    }
}

/// 刷新进度模式：轮询进度文件，结束则回收子进程并返回。
fn progress_tick() {
    unsafe {
        // 看门狗：后台任务超时（120s 无进展）自动取消，防止永久卡死。
        if now_us().saturating_sub(PROG_START_US) > 120_000_000 {
            progress_cancel();
            return;
        }
        let (stage, _done, _total) = read_progress();
        if stage == 1 || stage == 2 {
            // 等待子进程退出。
            let mut status = 0i32;
            syscall4(SYS_WAIT4, PROG_PID as usize, &mut status as *mut i32 as usize, 0, 0);
            PROG_PID = -1;
            UI_MODE = MODE_BROWSE;
            if stage == 2 {
                show_error("操作失败".as_bytes());
            } else {
                show_error(b"");
            }
            CLIP_OP = CLIP_NONE;
            load_dir(cwd_nul());
            redraw();
        }
    }
}

/// 取消后台任务。
fn progress_cancel() {
    unsafe {
        if PROG_PID > 0 {
            syscall2(SYS_KILL, PROG_PID as usize, 9);
            let mut status = 0i32;
            syscall4(SYS_WAIT4, PROG_PID as usize, &mut status as *mut i32 as usize, 0, 0);
            PROG_PID = -1;
        }
        UI_MODE = MODE_BROWSE;
        CLIP_OP = CLIP_NONE;
        show_error("已取消（目标可能留有部分内容）".as_bytes());
        load_dir(cwd_nul());
        redraw();
    }
}

// ==================== 点击处理 ====================

/// 工具栏命中：返回操作 id（0=粘贴/新建，1=取消/复制，2=新建/移动，3=复制/重命名，4=移动/删除）。
fn toolbar_hit(x: i32, y: i32) -> Option<usize> {
    unsafe {
        if y < TOOL_Y || y >= TOOL_Y + TOOL_H {
            return None;
        }
        let pending = CLIP_OP != CLIP_NONE;
        let gap = 8i32;
        let n = 5i32;
        let sw = (DISPLAY_W as i32 - 16 - gap * (n - 1)) / n;
        for i in 0..5i32 {
            let bx = 8 + i * (sw + gap);
            if x >= bx && x < bx + sw {
                let slot = i as usize;
                let act = if pending {
                    match slot {
                        0 => 10, // 粘贴
                        1 => 11, // 取消剪贴板
                        2 => 0,  // 新建
                        3 => 4,  // 复制（设置剪贴板）
                        4 => 5,  // 移动
                        _ => 99,
                    }
                } else {
                    match slot {
                        0 => 0, // 新建
                        1 => 4, // 复制
                        2 => 5, // 移动
                        3 => 6, // 重命名
                        4 => 7, // 删除
                        _ => 99,
                    }
                };
                return Some(act);
            }
        }
    }
    None
}

/// 选中项存在性检查（"." / ".." / 无选择）。
fn sel_ok() -> bool {
    unsafe {
        if SEL_IDX == usize::MAX || SEL_IDX >= ENT_COUNT {
            return false;
        }
        let nm = &ENT_NAMES[SEL_IDX][..ENT_LEN[SEL_IDX]];
        nm != b"." && nm != b".."
    }
}

fn handle_browse_click(x: i32, y: i32) -> bool {
    unsafe {
        // 返回键：根目录退出，否则上级。
        if back_hit(x, y) {
            if nul_len(&CWD) > 1 {
                go_up();
                redraw();
                return false;
            }
            return true;
        }
        // 路径栏跳转。
        if y >= PATH_BAR_Y && y < PATH_BAR_Y + PATH_BAR_H {
            if let Some(_) = path_bar_hit(x) {
                load_dir(cwd_nul());
                redraw();
            }
            return false;
        }
        // 工具栏。
        if y >= TOOL_Y && y < TOOL_Y + TOOL_H {
            if let Some(act) = toolbar_hit(x, y) {
                match act {
                    10 => {
                        do_paste();
                        redraw();
                    }
                    11 => {
                        CLIP_OP = CLIP_NONE;
                        show_error(b"");
                        redraw();
                    }
                    0 => {
                        INPUT_ACTION = ACT_NEW_FOLDER;
                        INPUT_BUF.fill(0);
                        INPUT_LEN = 0;
                        UI_MODE = MODE_INPUT;
                        redraw();
                    }
                    4 => {
                        if !sel_ok() {
                            show_error("请先选择项目".as_bytes());
                            redraw();
                            return false;
                        }
                        let mut full = [0u8; MAX_PATH];
                        let fl = sel_full_path(&mut full);
                        CLIP_OP = CLIP_COPY;
                        CLIP_LEN = fl;
                        CLIP_PATH.fill(0);
                        let mut n = 0usize;
                        while n < fl && n + 1 < CLIP_PATH.len() {
                            CLIP_PATH[n] = full[n];
                            n += 1;
                        }
                        CLIP_PATH[n] = 0;
                        CLIP_NAME_LEN = ENT_LEN[SEL_IDX];
                        CLIP_NAME[..CLIP_NAME_LEN]
                            .copy_from_slice(&ENT_NAMES[SEL_IDX][..ENT_LEN[SEL_IDX]]);
                        show_error(b"");
                        CLIP_OP = CLIP_COPY; // 保持操作类型（show_error 不动）。
                        redraw();
                    }
                    5 => {
                        if !sel_ok() {
                            show_error("请先选择项目".as_bytes());
                            redraw();
                            return false;
                        }
                        let mut full = [0u8; MAX_PATH];
                        let fl = sel_full_path(&mut full);
                        CLIP_OP = CLIP_MOVE;
                        CLIP_LEN = fl;
                        CLIP_PATH.fill(0);
                        let mut n = 0usize;
                        while n < fl && n + 1 < CLIP_PATH.len() {
                            CLIP_PATH[n] = full[n];
                            n += 1;
                        }
                        CLIP_PATH[n] = 0;
                        CLIP_NAME_LEN = ENT_LEN[SEL_IDX];
                        CLIP_NAME[..CLIP_NAME_LEN]
                            .copy_from_slice(&ENT_NAMES[SEL_IDX][..ENT_LEN[SEL_IDX]]);
                        show_error(b"");
                        CLIP_OP = CLIP_MOVE;
                        redraw();
                    }
                    6 => {
                        if !sel_ok() {
                            show_error("请先选择项目".as_bytes());
                            redraw();
                            return false;
                        }
                        INPUT_ACTION = ACT_RENAME;
                        INPUT_BUF.fill(0);
                        let n = ENT_LEN[SEL_IDX];
                        INPUT_BUF[..n].copy_from_slice(&ENT_NAMES[SEL_IDX][..n]);
                        INPUT_LEN = n;
                        INPUT_TARG_LEN = sel_full_path(&mut INPUT_TARG);
                        UI_MODE = MODE_INPUT;
                        redraw();
                    }
                    7 => {
                        if !sel_ok() {
                            show_error("请先选择项目".as_bytes());
                            redraw();
                            return false;
                        }
                        CONFIRM_ACTION = ACT_DELETE;
                        CONFIRM_PATH_LEN = sel_full_path(&mut CONFIRM_PATH);
                        UI_MODE = MODE_CONFIRM;
                        redraw();
                    }
                    _ => {}
                }
            }
            return false;
        }
        // 列表。
        if y < LIST_TOP || y >= LIST_BOTTOM {
            return false;
        }
        let row = ((y - LIST_TOP) / ROW_H) as usize;
        let rows_visible = ((LIST_BOTTOM - LIST_TOP) / ROW_H) as usize;
        let idx = LIST_SCROLL.min(ENT_COUNT.saturating_sub(rows_visible)) + row;
        if idx >= ENT_COUNT {
            return false;
        }
        let nm = &ENT_NAMES[idx][..ENT_LEN[idx]];
        // 右侧选中圈。
        let cx = DISPLAY_W as i32 - 34;
        if x >= cx - 12 && x <= cx + 12 {
            SEL_IDX = if SEL_IDX == idx { usize::MAX } else { idx };
            redraw();
            return false;
        }
        if nm == b"." {
            return false;
        }
        if nm == b".." {
            go_up();
            redraw();
            return false;
        }
        let t = ENT_TYPE[idx];
        if t == DT_DIR {
            enter_dir(nm);
            redraw();
            return false;
        }
        // 设备/管道/块设备：打开查看器会阻塞在读上，拒绝并提示。
        if t != DT_REG && t != DT_LNK {
            show_error("设备/管道文件无法打开".as_bytes());
            redraw();
            return false;
        }
        // 文件：文本查看 / 关联应用。
        let mut full = [0u8; MAX_PATH];
        let fl = join_into(&CWD[..nul_len(&CWD)], nm, &mut full);
        if fl > 0 {
            open_file(&full[..fl], nm);
        }
        false
    }
}

fn handle_viewer_click(x: i32, y: i32) {
    unsafe {
        if back_hit(x, y) {
            snapshot_page();
            begin_transition(false);
            UI_MODE = MODE_BROWSE;
            return;
        }
        let (total_lines, rows) = viewer_lines();
        let max_scroll = total_lines.saturating_sub(rows);
        if y < TITLE_H {
            return;
        }
        if y < DISPLAY_H as i32 / 2 {
            VIEW_SCROLL = VIEW_SCROLL.saturating_sub(rows / 2);
        } else {
            VIEW_SCROLL = (VIEW_SCROLL + rows / 2).min(max_scroll);
        }
    }
}

fn handle_input_click(x: i32, y: i32) {
    unsafe {
        let w = dialog::DLG_W;
        let h = DLG_INPUT_H;
        let (px, py) = dialog::center_rect(DISPLAY_W as i32, DISPLAY_H as i32, w, h);
        match dialog::hit_buttons(px, py + h - dialog::DLG_BTN_H, w, x, y) {
            Some(false) => {
                // 取消。
                UI_MODE = MODE_BROWSE;
                redraw();
            }
            Some(true) => {
                // 确定。
                do_input_action();
                redraw();
            }
            None => {
                // 点卡片外 = 取消。
                if dialog::outside(x, y, px, py, w, h) {
                    UI_MODE = MODE_BROWSE;
                    redraw();
                }
            }
        }
    }
}

fn handle_confirm_click(x: i32, y: i32) {
    unsafe {
        let w = dialog::DLG_W;
        let h = DLG_CONFIRM_H;
        let (px, py) = dialog::center_rect(DISPLAY_W as i32, DISPLAY_H as i32, w, h);
        match dialog::hit_buttons(px, py + h - dialog::DLG_BTN_H, w, x, y) {
            Some(false) => {
                UI_MODE = MODE_BROWSE;
                redraw();
            }
            Some(true) => {
                do_delete();
                redraw();
            }
            None => {
                if dialog::outside(x, y, px, py, w, h) {
                    UI_MODE = MODE_BROWSE;
                    redraw();
                }
            }
        }
    }
}

fn handle_progress_click(x: i32, y: i32) {
    unsafe {
        let w = dialog::DLG_W;
        let h = DLG_PROGRESS_H;
        let (px, py) = dialog::center_rect(DISPLAY_W as i32, DISPLAY_H as i32, w, h);
        // 进度进行中仅响应「取消」按钮（单按钮行：整行宽均命中）。
        if matches!(
            dialog::hit_buttons(px, py + h - dialog::DLG_BTN_H, w, x, y),
            Some(_)
        ) {
            progress_cancel();
        }
    }
}

// ==================== 键盘 ====================

/// 输入对话框按键。
fn handle_input_key(k: KeyAction) {
    unsafe {
        match k {
            KeyAction::Char(c) => {
                if c >= 0x20 && c < 0x7f && INPUT_LEN + 1 < MAX_NAME {
                    INPUT_BUF[INPUT_LEN] = c;
                    INPUT_LEN += 1;
                    redraw();
                }
            }
            KeyAction::Backspace => {
                if INPUT_LEN > 0 {
                    INPUT_LEN -= 1;
                    redraw();
                }
            }
            KeyAction::Enter => {
                do_input_action();
                redraw();
            }
            KeyAction::Esc => {
                UI_MODE = MODE_BROWSE;
                redraw();
            }
            _ => {}
        }
    }
}

// ==================== 主流程 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[files] 启动中\n");

    if !fb_init() {
        print("[files] /dev/fb0 内存映射失败，无帧缓冲\n");
        unsafe { exit(1); }
    }
    print("[files] 帧缓冲已映射\n");

    unsafe { CWD[0] = b'/'; }
    load_dir(cwd_nul());
    redraw();
    ttf_text::font_init();
    redraw();
    print("[files] 就绪\n");

    loop {
        let ev = poll_input();
        if ev.moved {
            cursor_overlay_move();
        }
        if ev.nav != NavAction::None {
            let code = nav_exit_code(ev.nav);
            if ui_mode_back(ev.nav) {
                redraw();
                sleep_ms(16);
                continue;
            }
            unsafe { exit(code); }
        }

        if unsafe { UI_MODE } == MODE_PROGRESS {
            progress_tick();
            if let Some((x, y)) = ev.tap {
                handle_progress_click(x, y);
            }
            match ev.key {
                KeyAction::Esc => progress_cancel(),
                _ => {}
            }
            redraw();
            sleep_ms(32);
            continue;
        }

        let mut exit_app = false;
        if let Some((x, y)) = ev.tap {
            match unsafe { UI_MODE } {
                MODE_VIEW => handle_viewer_click(x, y),
                MODE_INPUT => handle_input_click(x, y),
                MODE_CONFIRM => handle_confirm_click(x, y),
                _ => exit_app = handle_browse_click(x, y),
            }
            if exit_app {
                break;
            }
            redraw();
        }
        match ev.key {
            KeyAction::Esc => {
                match unsafe { UI_MODE } {
                    MODE_VIEW => {
                        unsafe {
                            snapshot_page();
                            begin_transition(false);
                            UI_MODE = MODE_BROWSE;
                        }
                        redraw();
                    }
                    MODE_INPUT | MODE_CONFIRM => {
                        unsafe { UI_MODE = MODE_BROWSE; }
                        redraw();
                    }
                    _ => {
                        if unsafe { nul_len(&CWD) > 1 } {
                            go_up();
                            redraw();
                        } else {
                            break;
                        }
                    }
                }
            }
            KeyAction::Enter | KeyAction::Char(_) | KeyAction::Backspace => {
                if unsafe { UI_MODE } == MODE_INPUT {
                    handle_input_key(ev.key);
                }
            }
            _ => {}
        }
        // 页面转场进行中：逐帧重绘推进（240ms FastOutSlowIn）。
        if transition_active() {
            redraw();
        }
        sleep_ms(16);
    }

    print("[files] 正在退出\n");
    unsafe { exit(0); }
}

/// 系统 Nav 返回：查看器/对话框逐级退出；浏览则上级目录/退出。
fn ui_mode_back(nav: NavAction) -> bool {
    if nav != NavAction::Back {
        return false;
    }
    unsafe {
        match UI_MODE {
            MODE_VIEW => {
                snapshot_page();
                begin_transition(false);
                UI_MODE = MODE_BROWSE;
            }
            MODE_INPUT | MODE_CONFIRM => {
                UI_MODE = MODE_BROWSE;
            }
            MODE_PROGRESS => {
                progress_cancel();
            }
            _ => {
                if nul_len(&CWD) > 1 {
                    go_up();
                    return true;
                }
                return false;
            }
        }
    }
    redraw();
    true
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
        write_fd(2, b"PANIC: files @ ");
        if let Some(loc) = info.location() {
            let f = loc.file().as_bytes();
            let fl = f.len().min(48);
            write_fd(2, &f[f.len() - fl..]);
            write_fd(2, b":");
            let mut nb = [0u8; 8];
            let mut i = nb.len();
            let mut v = loc.line() as u64;
            while v > 0 {
                i -= 1;
                nb[i] = b'0' + (v % 10) as u8;
                v /= 10;
            }
            write_fd(2, &nb[i..]);
        }
        write_fd(2, b"\n");
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}