//! 崩溃报告（tombstone 等价物）：SIGSEGV/SIGILL/内核 panic 时生成结构化报告，
//! 落盘到 `/data/crash/`（无 /data 时回退 `/tmp/crash/`），保留最近若干份。
//!
//! 报告内容：时间/进程/信号、寄存器转储、ESR/FAR 解码、内存映射
//! （`ProcPageTable.user_pages` 按地址聚合）、用户栈回溯（x29 帧指针链）、
//! 栈内容与内核日志尾部 —— 与 Android tombstone / debuggerd 的目标一致。
//!
//! 写入为 best-effort：VFS 不可用（早期启动 panic）时仅输出 UART。

use core::arch::asm;

// ==================== 信号 / ESR 解码 ====================

/// 由 ESR_EL1 的 EC 字段（bits[31:26]）推断信号与描述。
pub fn esr_describe(esr: u64) -> (i32, &'static str) {
    let ec = (esr >> 26) & 0x3F;
    let desc = match ec {
        0x00 => "未知原因 / 未定义指令",
        0x15 => "SVC（来自 EL0）",
        0x20 => "取指中止（来自 EL0）",
        0x21 => "取指中止（来自 EL1）",
        0x24 => "数据中止（来自 EL0）",
        0x25 => "数据中止（来自 EL1）",
        0x26 => "SP 对齐错误",
        0x30 => "断点（BRK，来自 EL0）",
        0x31 => "断点（BRK，来自 EL1）",
        0x32 => "软件单步（来自 EL0）",
        0x33 => "软件单步（来自 EL1）",
        0x34 => "监视点（来自 EL0）",
        0x3C => "BRK64（来自 EL0）",
        _ => "未知异常类别",
    };
    let sig = match ec {
        0x00 => 4,      // SIGILL
        0x20 | 0x24 | 0x25 => 11, // SIGSEGV
        0x26 => 7,      // SIGBUS
        0x30 | 0x31 | 0x3C | 0x32 | 0x33 => 5, // SIGTRAP
        0x34 => 5,
        _ => 11,
    };
    (sig, desc)
}

/// 信号号 → 名称。
pub fn signal_name(sig: i32) -> &'static str {
    match sig {
        4 => "SIGILL",
        5 => "SIGTRAP",
        7 => "SIGBUS",
        8 => "SIGFPE",
        11 => "SIGSEGV",
        6 => "SIGABRT",
        _ => "SIG?",
    }
}

// ==================== 故障信息 ====================

/// 一次崩溃的上下文（EL0 用户故障或内核 panic 通用）。
pub struct FaultInfo {
    /// 报告标题（"SIGSEGV" / "PANIC" 等）。
    pub kind: &'static str,
    /// 进程 pid。
    pub pid: u32,
    /// 进程/任务名。
    pub name: &'static str,
    pub esr: u64,
    pub elr: u64,
    pub far: u64,
}

// ==================== 寄存器视图 ====================

/// 寄存器快照：`x[0..31]` = x0..x30，`sp`、`pc`、`cpsr`。
pub struct Regs {
    pub x: [u64; 31],
    pub sp: u64,
    pub pc: u64,
    pub cpsr: u64,
}

/// 从 EL0 陷阱帧（`lower_sync` 保存，x0..x30 @0..30, sp @31, elr @32,
/// spsr @33）构造寄存器视图。
pub fn regs_from_trap_frame(frame: *const u64) -> Regs {
    let mut x = [0u64; 31];
    unsafe {
        for i in 0..31 {
            x[i] = *frame.add(i);
        }
    }
    Regs {
        x,
        sp: unsafe { *frame.add(31) },
        pc: unsafe { *frame.add(32) },
        cpsr: unsafe { *frame.add(33) },
    }
}

/// 从内核 PANIC_REGS（sync_entry 保存）+ 当前 sp 构造寄存器视图。
pub fn regs_from_panic_regs() -> Regs {
    let mut x = [0u64; 31];
    unsafe {
        for i in 0..31 {
            // PANIC_REGS[11] 是基址标记（sync_entry 用 x11 作基址），非真实 x11
            x[i] = crate::PANIC_REGS[i];
        }
    }
    let sp: u64;
    unsafe { asm!("mov {}, sp", out(reg) sp) };
    Regs { x, sp, pc: 0, cpsr: 0 }
}

// ==================== 报告构建 ====================

struct Buf {
    data: [u8; 8192],
    len: usize,
}

impl Buf {
    fn new() -> Buf {
        Buf { data: [0; 8192], len: 0 }
    }
    fn push(&mut self, s: &str) {
        let n = s.len().min(self.data.len().saturating_sub(self.len));
        self.data[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
    }
    fn push_hex(&mut self, v: u64) {
        let mut b = [0u8; 16];
        for i in 0..16 {
            let nib = ((v >> (60 - i * 4)) & 0xF) as u8;
            b[i] = if nib < 10 { b'0' + nib } else { b'a' + nib - 10 };
        }
        self.push(core::str::from_utf8(&b).unwrap_or(""));
    }
    fn push_dec(&mut self, mut v: u64) {
        let mut b = [0u8; 20];
        let mut i = b.len();
        loop {
            i -= 1;
            b[i] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        self.push(core::str::from_utf8(&b[i..]).unwrap_or(""));
    }
}

/// 构建完整 tombstone 文本到 `buf`。
fn build(buf: &mut Buf, fi: &FaultInfo, regs: &Regs, user: bool) {
    buf.push("*** *** *** *** *** *** *** *** *** *** *** *** *** *** *** ***\n");
    buf.push("KunxOS tombstone: ");
    buf.push(fi.kind);
    buf.push(" pid ");
    buf.push_dec(fi.pid as u64);
    buf.push(", name ");
    buf.push(fi.name);
    buf.push(", signal ");
    buf.push_dec(esr_describe(fi.esr).0 as u64);
    buf.push(" (");
    buf.push(signal_name(esr_describe(fi.esr).0));
    buf.push(")\n");
    buf.push("time: ");
    buf.push_dec(crate::timer::now_us() / 1000);
    buf.push(" ms\n");

    // ESR / FAR 解码
    buf.push("ESR:  0x");
    buf.push_hex(fi.esr);
    buf.push("  EC=");
    buf.push_dec((fi.esr >> 26) & 0x3F);
    buf.push("  ");
    buf.push(esr_describe(fi.esr).1);
    buf.push("\n");
    buf.push("ELR:  0x");
    buf.push_hex(fi.elr);
    buf.push("\n");
    buf.push("FAR:  0x");
    buf.push_hex(fi.far);
    buf.push("\n");

    // 寄存器
    buf.push("registers:\n");
    for row in 0..8 {
        for col in 0..4 {
            let idx = row * 4 + col;
            if idx < 31 {
                buf.push("  x");
                buf.push_dec(idx as u64);
                buf.push("=0x");
                buf.push_hex(regs.x[idx]);
            }
        }
        buf.push("\n");
    }
    buf.push("  sp=0x");
    buf.push_hex(regs.sp);
    buf.push("  pc=0x");
    buf.push_hex(regs.pc);
    buf.push("  cpsr=0x");
    buf.push_hex(regs.cpsr);
    buf.push("\n");

    // 内存映射（用户进程地址空间）
    if user {
        buf.push("memory map:\n");
        let mut i = 0usize;
        while i < 1_000_000 {
            // 用计数保护，防止记录表损坏时死循环
            let (va, pa, flags) = crate::process::user_map_at(crate::process::current_pid(), i);
            if pa == u64::MAX && flags == u64::MAX {
                break; // 表尾哨兵
            }
            // 聚合连续同权限页
            let start = va;
            let mut end = va;
            let mut j = i + 1;
            loop {
                let (va2, _pa2, flags2) = crate::process::user_map_at(crate::process::current_pid(), j);
                if va2 == u64::MAX && flags2 == u64::MAX {
                    break;
                }
                if va2 != end + 0x1000 || flags2 != flags {
                    break;
                }
                end = va2;
                j += 1;
            }
            buf.push("  0x");
            buf.push_hex(start);
            buf.push("-0x");
            buf.push_hex(end + 0xFFF);
            buf.push(" ");
            // 权限：AP 字段（bits[7:6]）+ UXN（bit54）
            let ap = (flags >> 6) & 0b11;
            let user_rw = ap == 0b01;
            let user_ro = ap == 0b11;
            let nx = (flags >> 54) & 1 == 1;
            buf.push(if user_rw { "rw" } else if user_ro { "r-" } else { "--" });
            buf.push(if nx { "-" } else { "x" });
            if flags & crate::vm::FLAG_COW != 0 {
                buf.push(" (cow)");
            }
            if pa == crate::vm::LAZY_PAGE as u64 {
                buf.push(" (lazy)");
            }
            buf.push("\n");
            i = j;
        }
    }

    // 用户栈回溯：x29 帧指针链
    if user && regs.x[29] != 0 {
        let fp = regs.x[29];
        let sp = regs.sp;
        let root = crate::process::current_root();
        buf.push("backtrace:\n");
        let mut cur = fp;
        let mut steps = 0usize;
        unsafe { crate::mmu::uaccess_begin() };
        while steps < 16 {
            if cur < sp.saturating_sub(0x10000)
                || cur > sp.saturating_add(0x10000)
                || cur % 8 != 0
                || !crate::vm::user_range_mapped(root, cur as usize, 16)
            {
                break;
            }
            let prev = unsafe { *(cur as *const u64) };
            let lr = unsafe { *((cur + 8) as *const u64) };
            buf.push("  #");
            buf.push_dec(steps as u64);
            buf.push(" pc=0x");
            buf.push_hex(lr);
            buf.push("  fp=0x");
            buf.push_hex(cur);
            buf.push("\n");
            if prev <= cur || prev > cur + 0x10000 {
                break;
            }
            cur = prev;
            steps += 1;
        }
        unsafe { crate::mmu::uaccess_end() };
    }

    // 用户栈内容（故障 sp 附近）
    if user {
        let root = crate::process::current_root();
        buf.push("stack:\n");
        unsafe { crate::mmu::uaccess_begin() };
        let start = regs.sp.saturating_sub(128);
        for off in (0..256usize).step_by(8) {
            let va = start + off as u64;
            if crate::vm::user_range_mapped(root, va as usize, 8) {
                let v = unsafe { *((va as usize) as *const u64) };
                buf.push("  0x");
                buf.push_hex(va);
                buf.push(": 0x");
                buf.push_hex(v);
                buf.push("\n");
            }
        }
        unsafe { crate::mmu::uaccess_end() };
    }

    // 内核日志尾部（结构化 log 最近记录）
    buf.push("内核日志尾部：\n");
    let mut logbuf = [0u8; 1024];
    let n = crate::log::log_snapshot(&mut logbuf);
    let tail = &logbuf[n.saturating_sub(768)..n];
    buf.push(core::str::from_utf8(tail).unwrap_or(""));
    buf.push("*** end tombstone ***\n");
}

// ==================== 落盘 ====================

/// 崩溃目录（优先 /data 持久化分区，回退 /tmp tmpfs）。
fn crash_dir() -> &'static str {
    if crate::vfs::stat_path("/data").is_some() {
        "/data/crash"
    } else if crate::vfs::stat_path("/tmp").is_some() {
        "/tmp/crash"
    } else {
        ""
    }
}

/// 尽力写文件：返回是否成功。
fn write_file(path: &str, data: &[u8]) -> bool {
    let fd = crate::vfs::open(path, crate::vfs::O_CREAT | crate::vfs::O_WRONLY | crate::vfs::O_TRUNC);
    if fd < 0 {
        crate::uart_send_str("[crash] open ");
        crate::uart_send_str(path);
        crate::uart_send_str(" 失败 err=");
        crate::print_dec(fd as u64);
        crate::uart_send_str("\n");
        return false;
    }
    let mut ok = true;
    let mut off = 0usize;
    while off < data.len() {
        let n = crate::vfs::write(fd, &data[off..]);
        if n <= 0 {
            crate::uart_send_str("[crash] write 失败于 off=");
            crate::print_dec(off as u64);
            crate::uart_send_str(" err=");
            crate::print_dec(n as u64);
            crate::uart_send_str("\n");
            ok = false;
            break;
        }
        off += n as usize;
    }
    crate::vfs::close(fd);
    ok
}

/// 保留最近 `KEEP` 份 tombstone（SFS 目录项按创建顺序，删除最早者）。
const KEEP: usize = 8;
fn rotate(dir: &str) {
    let mut name = [0u8; 32];
    let mut idx = 0usize;
    let mut count = 0usize;
    // 先数一遍
    loop {
        let ino = crate::vfs::readdir(dir, idx, &mut name);
        if ino < 0 {
            break;
        }
        idx += 1;
        let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        let nm = core::str::from_utf8(&name[..len]).unwrap_or("");
        if nm != "." && nm != ".." && !nm.is_empty() {
            count += 1;
        }
    }
    while count > KEEP {
        // 删除第一个条目（最旧）
        let mut n2 = [0u8; 32];
        let mut i = 0usize;
        loop {
            let ino = crate::vfs::readdir(dir, i, &mut n2);
            if ino < 0 {
                break;
            }
            i += 1;
            let len = n2.iter().position(|&c| c == 0).unwrap_or(n2.len());
            let nm = core::str::from_utf8(&n2[..len]).unwrap_or("");
            if nm != "." && nm != ".." && !nm.is_empty() {
                let mut p = [0u8; 128];
                let mut n = 0usize;
                for &c in dir.as_bytes() {
                    if n + 1 >= p.len() {
                        break;
                    }
                    p[n] = c;
                    n += 1;
                }
                if n > 0 && p[n - 1] != b'/' && n + 1 < p.len() {
                    p[n] = b'/';
                    n += 1;
                }
                for &c in nm.as_bytes() {
                    if n + 1 >= p.len() {
                        break;
                    }
                    p[n] = c;
                    n += 1;
                }
                let ps = core::str::from_utf8(&p[..n]).unwrap_or("");
                crate::vfs::unlink(ps);
                count -= 1;
                break;
            }
        }
    }
}

/// 生成并落盘 tombstone，返回文件名（失败返回 None）。
fn emit(fi: &FaultInfo, regs: &Regs, user: bool) -> Option<[u8; 48]> {
    let mut b = Buf::new();
    build(&mut b, fi, regs, user);

    let dir = crash_dir();
    if dir.is_empty() {
        // VFS 不可用：仅控制台输出
        crate::uart_send_str("=== tombstone（无文件系统） ===\n");
        let s = core::str::from_utf8(&b.data[..b.len]).unwrap_or("");
        crate::uart_send_str(s);
        return None;
    }
    let mk = crate::vfs::mkdir(dir);
    if mk != 0 {
        crate::uart_send_str("[crash] mkdir ");
        crate::uart_send_str(dir);
        crate::uart_send_str(" err=");
        crate::print_dec(mk as u64);
        crate::uart_send_str("\n");
    }
    // 文件名：tombstone_<pid>_<ts>.txt
    let mut fname = [0u8; 48];
    let mut n = 0usize;
    for &c in b"tombstone_".iter() {
        fname[n] = c;
        n += 1;
    }
    let ts = crate::timer::now_us() / 1000;
    let mut tb = [0u8; 20];
    let mut i = tb.len();
    let mut v = ts;
    loop {
        i -= 1;
        tb[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    for &c in &tb[i..] {
        if n < fname.len() {
            fname[n] = c;
            n += 1;
        }
    }
    for &c in b"_pid".iter() {
        if n < fname.len() {
            fname[n] = c;
            n += 1;
        }
    }
    let pid = fi.pid;
    let mut pb = [0u8; 10];
    let mut pi = pb.len();
    let mut pv = pid as u64;
    loop {
        pi -= 1;
        pb[pi] = b'0' + (pv % 10) as u8;
        pv /= 10;
        if pv == 0 {
            break;
        }
    }
    for &c in &pb[pi..] {
        if n < fname.len() {
            fname[n] = c;
            n += 1;
        }
    }
    for &c in b".txt".iter() {
        if n < fname.len() {
            fname[n] = c;
            n += 1;
        }
    }

    let mut path = [0u8; 128];
    let mut pn = 0usize;
    for &c in dir.as_bytes() {
        if pn + 1 >= path.len() {
            break;
        }
        path[pn] = c;
        pn += 1;
    }
    if pn > 0 && path[pn - 1] != b'/' && pn + 1 < path.len() {
        path[pn] = b'/';
        pn += 1;
    }
    for &c in &fname[..n] {
        if pn + 1 >= path.len() {
            break;
        }
        path[pn] = c;
        pn += 1;
    }
    let ps = core::str::from_utf8(&path[..pn]).unwrap_or("");
    let ok = write_file(ps, &b.data[..b.len]);
    if ok {
        rotate(dir);
        crate::printk!(crate::log::LOG_ERR, "crash", "崩溃转储已保存：{}", ps);
    }
    Some(fname)
}

// ==================== 公共入口 ====================

/// EL0 用户故障入口：从陷阱帧生成 tombstone 并落盘。
/// 由 `el0_fault_handler` 调用（当前进程 = 崩溃进程）。
pub fn el0_fault(fi: &FaultInfo, frame: *const u64) {
    let regs = regs_from_trap_frame(frame);
    emit(fi, &regs, true);
    // 控制台摘要
    crate::uart_send_str("[crash] tombstone: pid=");
    crate::print_dec(fi.pid as u64);
    crate::uart_send_str(" name=");
    crate::uart_send_str(fi.name);
    crate::uart_send_str(" sig=");
    crate::print_dec(esr_describe(fi.esr).0 as u64);
    crate::uart_send_str(" (");
    crate::uart_send_str(signal_name(esr_describe(fi.esr).0));
    crate::uart_send_str(") pc=0x");
    crate::print_hex(fi.elr);
    crate::uart_send_str("\n");
}

/// 内核 panic 入口：从 PANIC_REGS + 当前 sp 生成 tombstone。
pub fn kernel_panic(fi: &FaultInfo) {
    // 递归保护：panic 处理路径（tombstone 落盘）若再次触发 panic（如 SFS
    // 元数据损坏导致 vfs 调用 panic），直接放弃落盘，避免递归刷屏挂死。
    // 首次进入置位，退出（或放弃）时复位。
    static mut IN_PANIC: bool = false;
    unsafe {
        if IN_PANIC {
            crate::uart_send_str("[crash] 已跳过崩溃转储（递归 panic）\n");
            return;
        }
        IN_PANIC = true;
    }
    // 记录到结构化日志（手工格式化，避免依赖额外缓冲）
    let mut msg = [0u8; 96];
    let mut n = 0usize;
    for &c in b"panic ".iter() {
        msg[n] = c;
        n += 1;
    }
    for &c in fi.kind.as_bytes() {
        if n < msg.len() {
            msg[n] = c;
            n += 1;
        }
    }
    for &c in b" pid=".iter() {
        if n < msg.len() {
            msg[n] = c;
            n += 1;
        }
    }
    let mut pb = [0u8; 10];
    let mut pi = pb.len();
    let mut pv = fi.pid as u64;
    loop {
        pi -= 1;
        pb[pi] = b'0' + (pv % 10) as u8;
        pv /= 10;
        if pv == 0 {
            break;
        }
    }
    for &c in &pb[pi..] {
        if n < msg.len() {
            msg[n] = c;
            n += 1;
        }
    }
    crate::log::log_write(
        crate::log::LOG_EMERG,
        "kernel",
        core::str::from_utf8(&msg[..n]).unwrap_or("panic"),
    );
    let regs = regs_from_panic_regs();
    let elr: u64;
    unsafe { asm!("mrs {}, elr_el1", out(reg) elr) };
    let fi2 = FaultInfo {
        kind: fi.kind,
        pid: fi.pid,
        name: fi.name,
        esr: fi.esr,
        elr: if fi.elr != 0 { fi.elr } else { elr },
        far: fi.far,
    };
    emit(&fi2, &regs, false);
    unsafe { IN_PANIC = false; }
}
