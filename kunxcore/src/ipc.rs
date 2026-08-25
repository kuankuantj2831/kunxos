//! 进程间通信（IPC）：管道、消息队列、共享内存。
//!
//! - 管道：字节流环形缓冲，读写两端分别用文件描述符表示，写满/读空时阻塞。
//! - 消息队列：固定长度消息（含优先级），`recv` 返回当前最高优先级消息，
//!   同优先级按 FIFO；队列满（发送）/空（接收）时阻塞。
//! - 共享内存：分配一个物理页，可被多个进程映射到各自地址空间（物理页多映射）。
//!
//! 阻塞语义依赖调度器的 `block_current` / `wake`：阻塞前在对象中登记等待线程，
//! 对端操作后唤醒。

use core::arch::asm;

use crate::process::{self, FdKind};
use crate::sched_cfs;
use crate::vm;

/// 管道环形缓冲大小（字节）。
pub const PIPE_BUF: usize = 256;
const MAX_PIPES: usize = 16;

/// 每个对象同时可登记的等待线程数（多读/多写场景）。
const WAITERS: usize = 4;

/// 消息固定长度（含数据字节数）。
pub const MSG_SIZE: usize = 32;
/// 每个消息队列的槽位数。
const MSG_SLOTS: usize = 8;
const MAX_QUEUES: usize = 8;

/// 共享内存对象上限。
const MAX_SHMS: usize = 8;

/// 文件描述符访问标志：读端 / 写端。
pub const FD_READ: u32 = 0x1;
pub const FD_WRITE: u32 = 0x2;

// ==================== 等待者辅助 ====================

/// 登记一个等待线程；空位不足返回 false（调用方应避免阻塞，直接返回忙）。
fn wait_reg(list: &mut [Option<u32>; WAITERS], tid: u32) -> bool {
    for s in list.iter_mut() {
        if s.is_none() {
            *s = Some(tid);
            return true;
        }
    }
    false
}

/// 唤醒并清除全部等待者（调用方须已屏蔽 IRQ：使用 `wake_irq` 避免中途开中断）。
fn wait_wake(list: &mut [Option<u32>; WAITERS]) {
    for s in list.iter_mut() {
        if let Some(t) = s.take() {
            sched_cfs::wake_irq(t);
        }
    }
}

// ==================== 管道 ====================

/// 管道：环形缓冲字节流。
struct Pipe {
    used: bool,
    buf: [u8; PIPE_BUF],
    head: usize, // 读指针
    tail: usize, // 写指针
    count: usize, // 当前缓冲字节数
    /// 引用计数：指向本管道的进程 fd 数量（创建 = 两端 2，fork/dup 复制 +1，
    /// close/exit -1，归零才释放槽位 —— 防止 fork 子进程退出时把父进程仍在
    /// 使用的管道整槽清空，也防止 close 泄漏槽位）。
    refs: u32,
    reader_wait: [Option<u32>; WAITERS], // 阻塞中的读线程（多等待者）
    writer_wait: [Option<u32>; WAITERS], // 阻塞中的写线程（多等待者）
}

impl Pipe {
    const fn empty() -> Self {
        Pipe {
            used: false,
            buf: [0; PIPE_BUF],
            head: 0,
            tail: 0,
            count: 0,
            refs: 0,
            reader_wait: [None; WAITERS],
            writer_wait: [None; WAITERS],
        }
    }
}

static mut PIPES: [Pipe; MAX_PIPES] = [const { Pipe::empty() }; MAX_PIPES];

// ==================== 消息队列 ====================

/// 单条消息（定长 + 优先级）。
#[derive(Clone, Copy)]
struct Msg {
    used: bool,
    data: [u8; MSG_SIZE],
    len: usize,
    prio: i32,
}

const EMPTY_MSG: Msg = Msg { used: false, data: [0; MSG_SIZE], len: 0, prio: 0 };

/// 消息队列：固定槽位 + 优先级。
struct MsgQueue {
    used: bool,
    msgs: [Msg; MSG_SLOTS],
    /// 引用计数：指向本队列的进程 fd 数量（创建 = 1，fork/dup 复制 +1，
    /// close/exit -1，归零才释放槽位 —— 与管道同源的多进程生命周期问题）。
    refs: u32,
    recv_wait: [Option<u32>; WAITERS], // 阻塞中的接收线程（多等待者）
    send_wait: [Option<u32>; WAITERS], // 阻塞中的发送线程（多等待者）
}

impl MsgQueue {
    const fn empty() -> Self {
        MsgQueue {
            used: false,
            msgs: [EMPTY_MSG; MSG_SLOTS],
            refs: 0,
            recv_wait: [None; WAITERS],
            send_wait: [None; WAITERS],
        }
    }
}

static mut QUEUES: [MsgQueue; MAX_QUEUES] = [const { MsgQueue::empty() }; MAX_QUEUES];

// ==================== 共享内存 ====================

/// 每个共享内存对象可同时记录的活跃映射数（(pid, vaddr) 对）。
const MAX_SHM_MAPS: usize = 4;

/// 共享内存：一个物理页 + 引用计数 + 映射记录。
///
/// `refs` = fd 引用（创建 1 + fork/dup 复制 +1）+ 映射引用（每次 `shm_map` +1）。
/// `maps` 记录每个映射（进程 pid → 用户 vaddr），使 `close(fd)` 时能解除
/// 本进程的页表映射——否则 close 归零释放物理页后，进程页表仍引用该页
/// （use-after-free，H-15 缺口）。
struct Shm {
    used: bool,
    page: usize, // 物理页地址
    refs: u32,
    maps: [(u32, usize); MAX_SHM_MAPS], // (pid, vaddr)，pid 0 = 空槽
}

impl Shm {
    const fn empty() -> Self {
        Shm { used: false, page: 0, refs: 0, maps: [(0, 0); MAX_SHM_MAPS] }
    }
}

static mut SHMS: [Shm; MAX_SHMS] = [const { Shm::empty() }; MAX_SHMS];

// ==================== 初始化 ====================

/// 初始化 IPC 对象池（静态数组位于 .bss，默认已清零，此处显式复位保证幂等）。
pub fn init() {
    unsafe {
        for i in 0..MAX_PIPES {
            PIPES[i] = Pipe::empty();
        }
        for i in 0..MAX_QUEUES {
            QUEUES[i] = MsgQueue::empty();
        }
        for i in 0..MAX_SHMS {
            SHMS[i] = Shm::empty();
        }
    }
}

/// 分配空闲管道槽位；池满返回 `None`（不再 panic，防 DoS）。
fn alloc_pipe() -> Option<usize> {
    unsafe {
        for i in 0..MAX_PIPES {
            if !PIPES[i].used {
                PIPES[i].used = true;
                return Some(i);
            }
        }
    }
    None
}

/// 分配空闲消息队列槽位；池满返回 `None`。
fn alloc_queue() -> Option<usize> {
    unsafe {
        for i in 0..MAX_QUEUES {
            if !QUEUES[i].used {
                QUEUES[i].used = true;
                return Some(i);
            }
        }
    }
    None
}

/// 分配空闲共享内存槽位；池满返回 `None`。
fn alloc_shm() -> Option<usize> {
    unsafe {
        for i in 0..MAX_SHMS {
            if !SHMS[i].used {
                SHMS[i].used = true;
                return Some(i);
            }
        }
    }
    None
}

// ==================== 管道接口 ====================

/// 创建管道，返回（读端 fd，写端 fd），均登记到当前进程 fd 表；
/// 池满返回 `(-1, -1)`。
pub fn pipe_create() -> (i32, i32) {
    let idx = match alloc_pipe() {
        Some(i) => i,
        None => return (-1, -1),
    };
    unsafe {
        PIPES[idx].refs = 2; // 读端 + 写端两个 fd
    }
    let rfd = process::alloc_fd(FdKind::Pipe, idx, FD_READ);
    let wfd = process::alloc_fd(FdKind::Pipe, idx, FD_WRITE);
    if rfd < 0 || wfd < 0 {
        // fd 表满：回滚管道槽位（refs 归零），避免泄漏
        unsafe { PIPES[idx] = Pipe::empty(); }
        if rfd >= 0 {
            process::close_fd(rfd);
        }
        return (-1, -1);
    }
    (rfd, wfd)
}

/// 向管道写入 `data`，返回实际写入字节数（写满时阻塞，直至写完）。
///
/// 临界区全程屏蔽 IRQ：「检查缓冲 → 写入 → 唤醒读者」原子化，防止单核
/// 抢占下多读者/多写者并发读写环形缓冲；登记等待者与阻塞之间也屏蔽 IRQ，
/// 消除「登记后、阻塞前」丢唤醒的竞态窗口。
pub fn pipe_write(fd: i32, data: &[u8]) -> usize {
    let entry = match process::get_fd(fd) {
        Some(e) if e.kind == FdKind::Pipe => e,
        _ => return 0,
    };
    let idx = entry.index;
    unsafe { asm!("msr daifset, #2") };
    let mut written = 0usize;
    while written < data.len() {
        // 缓冲满：登记并阻塞（block_current 内部管理 DAIF，返回时恢复 IRQ）
        while unsafe { PIPES[idx].count == PIPE_BUF } {
            unsafe {
                if !wait_reg(&mut PIPES[idx].writer_wait, sched_cfs::current_tid()) {
                    asm!("msr daifclr, #2");
                    return written;
                }
            }
            sched_cfs::block_current();
            unsafe { asm!("msr daifset, #2") };
        }
        unsafe {
            let pipe = &mut PIPES[idx];
            pipe.buf[pipe.tail] = data[written];
            pipe.tail = (pipe.tail + 1) % PIPE_BUF;
            pipe.count += 1;
            // 唤醒全部等待中的读者（多读者场景不丢唤醒）
            wait_wake(&mut pipe.reader_wait);
        }
        written += 1;
    }
    unsafe { asm!("msr daifclr, #2") };
    written
}

/// 从管道读取数据：阻塞直到至少 1 字节可用，然后返回当前可读字节数
/// （至多 `out.len()` 字节，不会等到填满缓冲区）。
///
/// 与 [`pipe_write`] 相同：临界区屏蔽 IRQ，保证并发读写安全且不丢唤醒。
pub fn pipe_read(fd: i32, out: &mut [u8]) -> usize {
    let entry = match process::get_fd(fd) {
        Some(e) if e.kind == FdKind::Pipe => e,
        _ => return 0,
    };
    if out.is_empty() {
        return 0;
    }
    let idx = entry.index;
    unsafe { asm!("msr daifset, #2") };
    // 阻塞直到至少 1 字节
    while unsafe { PIPES[idx].count == 0 } {
        unsafe {
            if !wait_reg(&mut PIPES[idx].reader_wait, sched_cfs::current_tid()) {
                asm!("msr daifclr, #2");
                return 0;
            }
        }
        sched_cfs::block_current();
        unsafe { asm!("msr daifset, #2") };
    }
    // 读取当前全部可读字节（不超过 out.len()），原子完成
    let n = out.len().min(unsafe { PIPES[idx].count });
    unsafe {
        let pipe = &mut PIPES[idx];
        for i in 0..n {
            out[i] = pipe.buf[pipe.head];
            pipe.head = (pipe.head + 1) % PIPE_BUF;
        }
        pipe.count -= n;
        // 唤醒全部等待中的写者
        wait_wake(&mut pipe.writer_wait);
    }
    unsafe { asm!("msr daifclr, #2") };
    n
}

// ==================== 消息队列接口 ====================

/// 创建消息队列，返回 fd；池满返回 -1。
pub fn msgq_create() -> i32 {
    let idx = match alloc_queue() {
        Some(i) => i,
        None => return -1,
    };
    unsafe {
        QUEUES[idx].refs = 1;
    }
    let fd = process::alloc_fd(FdKind::MsgQueue, idx, FD_READ | FD_WRITE);
    if fd < 0 {
        unsafe { QUEUES[idx] = MsgQueue::empty(); }
        return -1;
    }
    fd
}

/// 发送一条消息（`data` 长度不超过 `MSG_SIZE`），返回发送字节数；队满时阻塞。
///
/// 临界区屏蔽 IRQ（同 [`pipe_write`]）。
pub fn msgq_send(fd: i32, data: &[u8], prio: i32) -> usize {
    let entry = match process::get_fd(fd) {
        Some(e) if e.kind == FdKind::MsgQueue => e,
        _ => return 0,
    };
    let len = data.len().min(MSG_SIZE);
    unsafe { asm!("msr daifset, #2") };
    loop {
        let slot = unsafe {
            let q = &mut QUEUES[entry.index];
            q.msgs.iter().position(|m| !m.used)
        };
        match slot {
            Some(s) => unsafe {
                let q = &mut QUEUES[entry.index];
                q.msgs[s].used = true;
                q.msgs[s].len = len;
                q.msgs[s].prio = prio;
                q.msgs[s].data[..len].copy_from_slice(&data[..len]);
                // 唤醒全部等待中的接收者（多接收者场景不丢唤醒）
                wait_wake(&mut q.recv_wait);
                asm!("msr daifclr, #2");
                return len;
            },
            None => {
                // 队满：登记并阻塞
                unsafe {
                    if !wait_reg(&mut QUEUES[entry.index].send_wait, sched_cfs::current_tid()) {
                        asm!("msr daifclr, #2");
                        return 0;
                    }
                }
                sched_cfs::block_current();
                unsafe { asm!("msr daifset, #2") };
            }
        }
    }
}

/// 接收一条消息（取当前最高优先级，同优先级 FIFO），返回消息长度；队空时阻塞。
///
/// 临界区屏蔽 IRQ（同 [`pipe_write`]）。
pub fn msgq_recv(fd: i32, out: &mut [u8]) -> (usize, i32) {
    let entry = match process::get_fd(fd) {
        Some(e) if e.kind == FdKind::MsgQueue => e,
        _ => return (0, 0),
    };
    unsafe { asm!("msr daifset, #2") };
    loop {
        // 找出最高优先级、且最早入队的消息
        let slot = unsafe {
            let q = &QUEUES[entry.index];
            let mut best: Option<usize> = None;
            for i in 0..MSG_SLOTS {
                if !q.msgs[i].used {
                    continue;
                }
                match best {
                    None => best = Some(i),
                    Some(b) => {
                        if q.msgs[i].prio > q.msgs[b].prio {
                            best = Some(i);
                        }
                    }
                }
            }
            best
        };
        match slot {
            Some(s) => unsafe {
                let q = &mut QUEUES[entry.index];
                let m = q.msgs[s];
                let n = m.len.min(out.len());
                out[..n].copy_from_slice(&m.data[..n]);
                q.msgs[s].used = false;
                // 唤醒全部等待中的发送者
                wait_wake(&mut q.send_wait);
                asm!("msr daifclr, #2");
                return (n, m.prio);
            },
            None => {
                unsafe {
                    if !wait_reg(&mut QUEUES[entry.index].recv_wait, sched_cfs::current_tid()) {
                        asm!("msr daifclr, #2");
                        return (0, 0);
                    }
                }
                sched_cfs::block_current();
                unsafe { asm!("msr daifset, #2") };
            }
        }
    }
}

// ==================== 共享内存接口 ====================

/// 创建共享内存（分配一个物理页），返回 fd；池满或内存不足返回 -1。
pub fn shm_create() -> i32 {
    let idx = match alloc_shm() {
        Some(i) => i,
        None => return -1,
    };
    let page = match crate::allocator::alloc_page() {
        Some(p) => p,
        None => {
            unsafe { SHMS[idx] = Shm::empty(); }
            return -1;
        }
    };
    unsafe {
        SHMS[idx].page = page;
        SHMS[idx].refs = 1;
    }
    let fd = process::alloc_fd(FdKind::Shm, idx, FD_READ | FD_WRITE);
    if fd < 0 {
        unsafe {
            crate::allocator::free_page(page);
            SHMS[idx] = Shm::empty();
        }
        return -1;
    }
    fd
}

/// 把共享内存物理页映射到当前进程的 `vaddr`（用户态 RW），返回物理页地址。
///
/// 这就是“物理页多映射”：同一个物理页可被不同进程映射到各自地址空间。
/// 每次映射视为一次引用（refs + 1）并记录 (pid, vaddr)，与 `shm_close`
/// 配对（close 时解除映射并 -1），避免「close 归零释放物理页但页表仍
/// 映射」的 use-after-free。
pub fn shm_map(fd: i32, vaddr: usize) -> usize {
    let entry = match process::get_fd(fd) {
        Some(e) if e.kind == FdKind::Shm => e,
        _ => return 0,
    };
    let pid = process::current_pid();
    let page = unsafe { SHMS[entry.index].page };
    if page == 0 {
        return 0;
    }
    unsafe {
        let mut recorded = false;
        for m in SHMS[entry.index].maps.iter_mut() {
            if m.0 == pid && m.1 == vaddr {
                recorded = true; // 本进程已映射同一地址：不重复计数
                break;
            }
            if m.0 == 0 {
                *m = (pid, vaddr);
                recorded = true;
                break;
            }
        }
        if recorded {
            SHMS[entry.index].refs = SHMS[entry.index].refs.saturating_add(1);
        }
    }
    process::map_user_page(vaddr, page);
    page
}

/// 把共享内存物理页映射到当前进程页表（等价 `shm_map`，按需调用）。
#[allow(dead_code)]
pub fn shm_attach(fd: i32, vaddr: usize) -> usize {
    shm_map(fd, vaddr)
}

/// 增加共享内存引用计数（供 fork 复制 fd 表时调用，防止子进程 close 导致提前释放）。
pub fn shm_add_ref(index: usize) {
    unsafe {
        if index < MAX_SHMS && SHMS[index].used {
            SHMS[index].refs = SHMS[index].refs.saturating_add(1);
        }
    }
}

/// 按下标释放一个共享内存引用（进程退出清理 fd 时调用）。
///
/// `pid` 为退出进程：其页表已随 `reap` 销毁，映射自动解除，此处只清
/// 映射记录（防 pid 复用后误 unmap）并减 fd 引用。
pub fn shm_close_index(pid: u32, index: usize) {
    if index >= MAX_SHMS {
        return;
    }
    unsafe {
        let s = &mut SHMS[index];
        if !s.used || s.refs == 0 {
            return;
        }
        // 清掉该进程的映射记录（页表已销毁，无需 unmap）
        for m in s.maps.iter_mut() {
            if m.0 == pid {
                m.0 = 0;
                m.1 = 0;
            }
        }
        s.refs -= 1;
        if s.refs == 0 {
            crate::allocator::free_page(s.page);
            *s = Shm::empty();
        }
    }
}

/// 增加管道引用计数（fork/dup 复制 fd 时调用，防止子进程退出清空父进程管道）。
pub fn pipe_add_ref(index: usize) {
    if index < MAX_PIPES {
        unsafe {
            if PIPES[index].used {
                PIPES[index].refs = PIPES[index].refs.saturating_add(1);
            }
        }
    }
}

/// 释放一个管道引用（close/exit 时调用），引用归零才清空槽位。
pub fn pipe_drop_ref(index: usize) {
    if index >= MAX_PIPES {
        return;
    }
    unsafe {
        let p = &mut PIPES[index];
        if !p.used {
            return;
        }
        if p.refs > 0 {
            p.refs -= 1;
        }
        if p.refs == 0 {
            *p = Pipe::empty();
        }
    }
}

/// 增加消息队列引用计数（fork/dup 复制 fd 时调用）。
pub fn msgq_add_ref(index: usize) {
    if index < MAX_QUEUES {
        unsafe {
            if QUEUES[index].used {
                QUEUES[index].refs = QUEUES[index].refs.saturating_add(1);
            }
        }
    }
}

/// 释放一个消息队列引用（close/exit 时调用），引用归零才清空槽位。
pub fn msgq_drop_ref(index: usize) {
    if index >= MAX_QUEUES {
        return;
    }
    unsafe {
        let q = &mut QUEUES[index];
        if !q.used {
            return;
        }
        if q.refs > 0 {
            q.refs -= 1;
        }
        if q.refs == 0 {
            *q = MsgQueue::empty();
        }
    }
}

/// 获取共享内存物理页地址（供内核态直接读写演示）。
pub fn shm_page(fd: i32) -> usize {
    let entry = match process::get_fd(fd) {
        Some(e) if e.kind == FdKind::Shm => e,
        _ => return 0,
    };
    unsafe { SHMS[entry.index].page }
}

/// 释放共享内存引用（`close(fd)` 时调用）。
///
/// 先解除本进程对该对象的全部页表映射（`maps` 记录中的 (pid, vaddr)，
/// unmap + 引用 -1），再减 fd 引用；引用归零才归还物理页 —— 保证物理页
/// 释放时没有任何进程页表仍引用它（use-after-free 修复）。
#[allow(dead_code)]
pub fn shm_close(fd: i32) {
    let entry = match process::get_fd(fd) {
        Some(e) if e.kind == FdKind::Shm => e,
        _ => return,
    };
    let pid = process::current_pid();
    let root = process::current_root();
    unsafe {
        let s = &mut SHMS[entry.index];
        if !s.used || s.refs == 0 {
            return;
        }
        // 解除本进程的映射（每次映射 -1）
        for m in s.maps.iter_mut() {
            if m.0 == pid {
                vm::unmap_page_in(root, m.1);
                m.0 = 0;
                m.1 = 0;
                if s.refs > 0 {
                    s.refs -= 1;
                }
            }
        }
        // fd 引用
        s.refs -= 1;
        if s.refs == 0 {
            crate::allocator::free_page(s.page);
            *s = Shm::empty();
        }
    }
}


// ==================== 套接字对（socketpair，双向字节流） ====================
//
// 每个 PairSock 含两条独立环形缓冲：side0→side1（ring_a）与 side1→side0
// （ring_b），实现全双工字节流。fd 的 flags 字段编码 side（0/1）。

/// 每方向缓冲大小。
const PAIR_BUF: usize = 2048;
/// 套接字对上限。
const MAX_PAIRS: usize = 8;

/// 单向环形缓冲（与 Pipe 同构）。
struct PairRing {
    buf: [u8; PAIR_BUF],
    head: usize,
    tail: usize,
    count: usize,
    read_wait: [Option<u32>; WAITERS],
    write_wait: [Option<u32>; WAITERS],
}

impl PairRing {
    const fn empty() -> Self {
        PairRing {
            buf: [0; PAIR_BUF],
            head: 0,
            tail: 0,
            count: 0,
            read_wait: [None; WAITERS],
            write_wait: [None; WAITERS],
        }
    }
}

/// 套接字对：两条方向相反的环。
struct PairSock {
    used: bool,
    /// side0 写 → side1 读
    ring_a: PairRing,
    /// side1 写 → side0 读
    ring_b: PairRing,
    /// 指向本对象的 fd 数（两个用户 fd）。
    refs: u32,
}

impl PairSock {
    const fn empty() -> Self {
        PairSock {
            used: false,
            ring_a: PairRing::empty(),
            ring_b: PairRing::empty(),
            refs: 0,
        }
    }
}

static mut PAIRS: [PairSock; MAX_PAIRS] = [const { PairSock::empty() }; MAX_PAIRS];

fn pair_alloc() -> Option<usize> {
    unsafe {
        for i in 0..MAX_PAIRS {
            if !PAIRS[i].used {
                PAIRS[i].used = true;
                return Some(i);
            }
        }
    }
    None
}

/// 创建套接字对，返回 `(fd0, fd1)`；失败返回 `(-1, -1)`。
pub fn socketpair_create() -> (i32, i32) {
    let idx = match pair_alloc() {
        Some(i) => i,
        None => return (-1, -1),
    };
    unsafe {
        PAIRS[idx].refs = 2;
    }
    let f0 = process::alloc_fd(FdKind::Pair, idx, 0);
    let f1 = process::alloc_fd(FdKind::Pair, idx, 1);
    if f0 < 0 || f1 < 0 {
        unsafe { PAIRS[idx] = PairSock::empty(); }
        if f0 >= 0 {
            process::close_fd(f0);
        }
        return (-1, -1);
    }
    (f0, f1)
}

/// 取 side 的写环（对方读）与读环（对方写）。
fn pair_rings(idx: usize, side: u32) -> (*mut PairRing, *mut PairRing) {
    unsafe {
        if side == 0 {
            (&mut PAIRS[idx].ring_a as *mut PairRing, &mut PAIRS[idx].ring_b as *mut PairRing)
        } else {
            (&mut PAIRS[idx].ring_b as *mut PairRing, &mut PAIRS[idx].ring_a as *mut PairRing)
        }
    }
}

fn ring_write(ring: &mut PairRing, data: &[u8], saved: u64) -> usize {
    // 调用方已屏蔽 IRQ；循环写入直至全部写入（写满时登记等待并阻塞）
    let mut written = 0usize;
    while written < data.len() {
        let n = (data.len() - written).min(PAIR_BUF - ring.count);
        if n == 0 {
            // 缓冲满：登记写等待并阻塞
            let cur = sched_cfs::current_tid();
            let mut reg = false;
            for w in ring.write_wait.iter_mut() {
                if w.is_none() {
                    *w = Some(cur);
                    reg = true;
                    break;
                }
            }
            if !reg {
                unsafe {
                    crate::sched_cfs::write_daif(saved);
                    crate::sched_cfs::yield_now();
                    crate::sched_cfs::write_daif(saved);
                }
                continue;
            }
            unsafe {
                crate::sched_cfs::write_daif(saved);
                sched_cfs::block_current();
                crate::sched_cfs::write_daif(saved);
            }
            continue;
        }
        for i in 0..n {
            ring.buf[(ring.tail + i) % PAIR_BUF] = data[written + i];
        }
        ring.tail = (ring.tail + n) % PAIR_BUF;
        ring.count += n;
        written += n;
        wait_wake(&mut ring.read_wait);
    }
    written
}

fn ring_read(ring: &mut PairRing, out: &mut [u8], saved: u64) -> usize {
    // 调用方已屏蔽 IRQ
    loop {
        if ring.count > 0 {
            let n = out.len().min(ring.count);
            for i in 0..n {
                out[i] = ring.buf[(ring.head + i) % PAIR_BUF];
            }
            ring.head = (ring.head + n) % PAIR_BUF;
            ring.count -= n;
            wait_wake(&mut ring.write_wait);
            return n;
        }
        // 空缓冲：登记读等待并阻塞
        let cur = sched_cfs::current_tid();
        let mut reg = false;
        for w in ring.read_wait.iter_mut() {
            if w.is_none() {
                *w = Some(cur);
                reg = true;
                break;
            }
        }
        if !reg {
            unsafe {
                crate::sched_cfs::write_daif(saved);
                crate::sched_cfs::yield_now();
                crate::sched_cfs::write_daif(saved);
            }
            continue;
        }
        unsafe {
            crate::sched_cfs::write_daif(saved);
            sched_cfs::block_current();
            crate::sched_cfs::write_daif(saved);
        }
    }
}

/// `socketpair` fd 写入：写入本方写环（对方读）。
pub fn pair_write(fd: i32, data: &[u8]) -> usize {
    let entry = match process::get_fd(fd) {
        Some(e) if e.kind == FdKind::Pair => e,
        _ => return 0,
    };
    if entry.index >= MAX_PAIRS {
        return 0;
    }
    let (wring, _rring) = pair_rings(entry.index, entry.flags);
    let saved = unsafe { crate::sched_cfs::read_daif() };
    unsafe {
        core::arch::asm!("msr daifset, #2");
        let n = ring_write(&mut *wring, data, saved);
        crate::sched_cfs::write_daif(saved);
        n
    }
}

/// `socketpair` fd 读取：从本方读环（对方写）取数据；空时阻塞。
pub fn pair_read(fd: i32, out: &mut [u8]) -> usize {
    let entry = match process::get_fd(fd) {
        Some(e) if e.kind == FdKind::Pair => e,
        _ => return 0,
    };
    if entry.index >= MAX_PAIRS {
        return 0;
    }
    let (_wring, rring) = pair_rings(entry.index, entry.flags);
    let saved = unsafe { crate::sched_cfs::read_daif() };
    unsafe {
        core::arch::asm!("msr daifset, #2");
        let n = ring_read(&mut *rring, out, saved);
        crate::sched_cfs::write_daif(saved);
        n
    }
}

/// `socketpair` fd 关闭：引用计数 -1，归零释放槽位。
pub fn pair_close(fd: i32) {
    if let Some(entry) = process::get_fd(fd) {
        if entry.kind == FdKind::Pair && entry.index < MAX_PAIRS {
            unsafe {
                let p = &mut PAIRS[entry.index];
                if p.refs > 0 {
                    p.refs -= 1;
                }
                if p.refs == 0 {
                    *p = PairSock::empty();
                }
            }
        }
    }
}
