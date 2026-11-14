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

/// Nonblocking readiness snapshot. Preserve the caller's IRQ state.
pub fn poll_events(e: process::FdEntry) -> i16 {
    let saved: u64;
    unsafe { asm!("csrrc {}, sstatus, {}", out(reg) saved, in(reg) 2u64); }
    let events = unsafe {
        match e.kind {
            FdKind::Pipe if e.index < MAX_PIPES => {
                let p = &PIPES[e.index];
                let mut r = 0;
                if e.flags & FD_READ != 0 {
                    if p.count > 0 || p.writers == 0 { r |= 0x001; }
                    if p.writers == 0 { r |= 0x010; }
                }
                if e.flags & FD_WRITE != 0 {
                    if p.readers == 0 { r |= 0x008; }
                    else if p.count < PIPE_BUF { r |= 0x004; }
                }
                r
            }
            FdKind::Pair if e.index < MAX_PAIRS => {
                let p = &PAIRS[e.index];
                let (read, write, peers) = if e.flags == 0 {
                    (p.ring_b.count, p.ring_a.count, p.side1)
                } else { (p.ring_a.count, p.ring_b.count, p.side0) };
                let mut r = 0;
                if read > 0 || peers == 0 { r |= 0x001; }
                if peers == 0 { r |= 0x010; }
                else if write < PAIR_BUF { r |= 0x004; }
                r
            }
            // Message queue flags/priority semantics are not poll-compatible yet.
            _ => 0x008,
        }
    };
    if saved & 2 != 0 { unsafe { asm!("csrsi sstatus, 2"); } }
    events
}

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
    /// 存活的**读端 fd** 数（EOF 判定）：写端全部关闭后读者醒来发现
    /// `readers == 0 && writers`（见下）……实际含义见注释：`readers`
    /// 统计仍打开的读端 fd，`writers` 统计仍打开的写端 fd。
    readers: u32,
    writers: u32,
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
            readers: 0,
            writers: 0,
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
    /// 全局入队序号：同优先级按 FIFO 选择（槽位复用会打乱下标序，
    /// 必须按序号判定先后）。
    seq: u64,
}

/// 全局消息序号计数器（msgq_send 内屏蔽 IRQ 下递增）。
static mut MSG_SEQ: u64 = 0;

const EMPTY_MSG: Msg = Msg { used: false, data: [0; MSG_SIZE], len: 0, prio: 0, seq: 0 };

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
        PIPES[idx].readers = 1;
        PIPES[idx].writers = 1;
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
    unsafe { asm!("csrci sstatus, 2") };
    let mut written = 0usize;
    while written < data.len() {
        // 缓冲满：登记并阻塞（block_current 内部管理 DAIF，返回时恢复 IRQ）
        while unsafe { PIPES[idx].count == PIPE_BUF } {
            unsafe {
                // ★ EPIPE 语义：所有读端 fd 已关闭（readers == 0）或槽位已清空
                // （used == false，refs 归零）时，写者不应再等待——此前只检查
                // 缓冲满、读者全部关闭后写者永久挂起。
                if !PIPES[idx].used || PIPES[idx].readers == 0 {
                    asm!("csrsi sstatus, 2");
                    return written;
                }
                if !wait_reg(&mut PIPES[idx].writer_wait, sched_cfs::current_tid()) {
                    // 等待者表满（≥5 并发等待者）：让出 CPU 后重试登记。
                    // 此前直接 return written —— written==0 时被调用方误判
                    // 为"写入 0 字节成功"，实际数据未写。
                    asm!("csrsi sstatus, 2");
                    sched_cfs::yield_now();
                    asm!("csrci sstatus, 2");
                    continue;
                }
            }
            sched_cfs::block_current();
            unsafe { asm!("csrci sstatus, 2") };
        }
        unsafe {
            let pipe = &mut PIPES[idx];
            if !pipe.used || pipe.readers == 0 {
                // 等待期间对端全部关闭：不再写入
                asm!("csrsi sstatus, 2");
                return written;
            }
            pipe.buf[pipe.tail] = data[written];
            pipe.tail = (pipe.tail + 1) % PIPE_BUF;
            pipe.count += 1;
            // 唤醒全部等待中的读者（多读者场景不丢唤醒）
            wait_wake(&mut pipe.reader_wait);
        }
        written += 1;
    }
    unsafe { asm!("csrsi sstatus, 2") };
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
    unsafe { asm!("csrci sstatus, 2") };
    // 阻塞直到至少 1 字节；所有写端关闭（writers == 0）或槽位清空 = EOF。
    while unsafe { PIPES[idx].count == 0 && PIPES[idx].used && PIPES[idx].writers > 0 } {
        unsafe {
            if !wait_reg(&mut PIPES[idx].reader_wait, sched_cfs::current_tid()) {
                // 等待者表满：让出 CPU 后重试登记。此前 return 0 会被应用
                // 当作 EOF（管道关闭），实际只是并发读者过多。
                asm!("csrsi sstatus, 2");
                sched_cfs::yield_now();
                asm!("csrci sstatus, 2");
                continue;
            }
        }
        sched_cfs::block_current();
        unsafe { asm!("csrci sstatus, 2") };
    }
    // EOF：槽位已清空或写端全部关闭且无数据 → 返回 0（read 的 EOF 语义）
    if unsafe { !PIPES[idx].used || (PIPES[idx].count == 0 && PIPES[idx].writers == 0) } {
        unsafe { asm!("csrsi sstatus, 2") };
        return 0;
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
    unsafe { asm!("csrsi sstatus, 2") };
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
/// 临界区屏蔽 IRQ（同 [`pipe_write`]）。每轮阻塞唤醒后重查槽位 `used`：
/// 队列在等待期间可能被最后一方 close 清空复用（此前把消息写进已关闭/
/// 复用的队列并报成功——假成功）。
pub fn msgq_send(fd: i32, data: &[u8], prio: i32) -> usize {
    let entry = match process::get_fd(fd) {
        Some(e) if e.kind == FdKind::MsgQueue => e,
        _ => return 0,
    };
    let len = data.len().min(MSG_SIZE);
    unsafe { asm!("csrci sstatus, 2") };
    loop {
        // 阻塞唤醒后槽位可能已被清空：按发送失败处理（0 字节）
        if !unsafe { QUEUES[entry.index].used } {
            unsafe { asm!("csrsi sstatus, 2") };
            return 0;
        }
        let slot = unsafe {
            let q = &mut QUEUES[entry.index];
            q.msgs.iter().position(|m| !m.used)
        };
        match slot {
            Some(s) => unsafe {
                let q = &mut QUEUES[entry.index];
                MSG_SEQ = MSG_SEQ.wrapping_add(1);
                q.msgs[s].used = true;
                q.msgs[s].len = len;
                q.msgs[s].prio = prio;
                q.msgs[s].seq = MSG_SEQ;
                q.msgs[s].data[..len].copy_from_slice(&data[..len]);
                // 唤醒全部等待中的接收者（多接收者场景不丢唤醒）
                wait_wake(&mut q.recv_wait);
                asm!("csrsi sstatus, 2");
                return len;
            },
            None => {
                // 队满：登记并阻塞
                unsafe {
                    if !wait_reg(&mut QUEUES[entry.index].send_wait, sched_cfs::current_tid()) {
                        // 等待者表满：让出重试。此前 return 0 被调用方当作
                        // "发送成功 0 字节"，实际消息未入队。
                        asm!("csrsi sstatus, 2");
                        sched_cfs::yield_now();
                        asm!("csrci sstatus, 2");
                        continue;
                    }
                }
                sched_cfs::block_current();
                unsafe { asm!("csrci sstatus, 2") };
            }
        }
    }
}

/// 接收一条消息（取当前最高优先级，同优先级 FIFO），返回消息长度；队空时阻塞。
///
/// 临界区屏蔽 IRQ（同 [`pipe_write`]）。每轮阻塞唤醒后重查槽位 `used`：
/// 队列被最后一方 close 清空时返回 EOF（(0, 0)），不再重新登记永久阻塞。
pub fn msgq_recv(fd: i32, out: &mut [u8]) -> (usize, i32) {
    let entry = match process::get_fd(fd) {
        Some(e) if e.kind == FdKind::MsgQueue => e,
        _ => return (0, 0),
    };
    unsafe { asm!("csrci sstatus, 2") };
    loop {
        if !unsafe { QUEUES[entry.index].used } {
            // 队列已关闭：EOF 语义
            unsafe { asm!("csrsi sstatus, 2") };
            return (0, 0);
        }
        // 找出最高优先级、同优先级最早入队（按 seq）的消息
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
                        if q.msgs[i].prio > q.msgs[b].prio
                            || (q.msgs[i].prio == q.msgs[b].prio && q.msgs[i].seq < q.msgs[b].seq)
                        {
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
                asm!("csrsi sstatus, 2");
                return (n, m.prio);
            },
            None => {
                unsafe {
                    if !wait_reg(&mut QUEUES[entry.index].recv_wait, sched_cfs::current_tid()) {
                        // 等待者表满：让出重试。此前 return (0,0) 被调用方
                        // 当作"空消息成功"，实际队非空只是并发读者过多。
                        asm!("csrsi sstatus, 2");
                        sched_cfs::yield_now();
                        asm!("csrci sstatus, 2");
                        continue;
                    }
                }
                sched_cfs::block_current();
                unsafe { asm!("csrci sstatus, 2") };
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
    // ★ 记录登记 + refs 更新是竞态临界区：syscall 上下文 IRQ 使能，tick
    // 抢占夹在 refs 读-改-写之间、与另一任务对同一 shm 的 map/close 并发，
    // 会丢一次递增/递减 → refs 提前归零释放仍被映射的物理页（UAF）。
    // 与 shm_close_index 同款屏蔽（本文件其余 add/drop 均有）。
    unsafe { asm!("csrci sstatus, 2") };
    unsafe {
        let mut recorded = false;
        let mut free_slot = false;
        for m in SHMS[entry.index].maps.iter_mut() {
            if m.0 == pid && m.1 == vaddr {
                recorded = true; // 同一进程同一地址重复映射：保持幂等，不重复计数
                break;
            }
            if m.0 == 0 {
                *m = (pid, vaddr);
                free_slot = true;
                break;
            }
        }
        // ★ 映射记录表已满（MAX_SHM_MAPS）且本 (pid,vaddr) 未登记：此前仍继续
        // 建立页表映射但不加入记录——刷新引用归零后该进程页表仍引用已释放的
        // 物理页（use-after-free）。记录表满时**拒绝本次映射**，不留无记录映射。
        if !recorded && !free_slot {
            unsafe { asm!("csrsi sstatus, 2") };
            return 0;
        }
        // 只对「新登记」+1：已登记（幂等重复映射）不重复计数——此前重复映射会
        // 虚增 refs，最终 shm 槽与物理页永不被释放（泄漏）。
        if free_slot {
            SHMS[entry.index].refs = SHMS[entry.index].refs.saturating_add(1);
        }
    }
    unsafe { asm!("csrsi sstatus, 2") };
    process::map_user_page(vaddr, page);
    page
}

/// 把共享内存物理页映射到当前进程页表（等价 `shm_map`，按需调用）。
#[allow(dead_code)]
pub fn shm_attach(fd: i32, vaddr: usize) -> usize {
    shm_map(fd, vaddr)
}

/// 只撤销一次 fd 引用（dup 失败回滚专用）。
///
/// ★ 不能用 `shm_close_index` 回滚：它按「进程已退出、页表已销毁」设计，
/// 会清光该进程的映射记录并按 1+maps 归还引用——回滚时进程活着且可能已
/// shm_map：记录被清但 PTE 仍在，后续 close 按空记录跳过 unmap 直接归零
/// 释放 → 用户态 vaddr 指向已归还的物理页（use-after-free）。
pub fn shm_drop_one_ref(index: usize) {
    if index >= MAX_SHMS {
        return;
    }
    unsafe { asm!("csrci sstatus, 2") };
    unsafe {
        let s = &mut SHMS[index];
        if s.used && s.refs > 0 {
            s.refs -= 1;
            if s.refs == 0 {
                crate::allocator::free_page(s.page);
                *s = Shm::empty();
            }
        }
    }
    unsafe { asm!("csrsi sstatus, 2") };
}

/// 增加共享内存引用计数（供 fork 复制 fd 表时调用，防止子进程 close 导致提前释放）。
pub fn shm_add_ref(index: usize) {
    unsafe { asm!("csrci sstatus, 2") };
    unsafe {
        if index < MAX_SHMS && SHMS[index].used {
            SHMS[index].refs = SHMS[index].refs.saturating_add(1);
        }
    }
    unsafe { asm!("csrsi sstatus, 2") };
}

/// 按下标释放共享内存引用（进程退出/回收清理 fd 时调用）。
///
/// `pid` 为退出进程：其页表已随 `reap` 销毁，映射自动解除，此处清
/// 映射记录并归还引用。★ 引用必须**逐映射记录**归还（`shm_map` 对每个
/// 新登记映射 +1）：此前只减 1 次 fd 引用，"create → map → exit" 一次
/// 即永久泄漏 1 个引用，MAX_SHMS 槽位耗尽后 shm_create 永久失败。
pub fn shm_close_index(pid: u32, index: usize) {
    if index >= MAX_SHMS {
        return;
    }
    unsafe { asm!("csrci sstatus, 2") };
    unsafe {
        let s = &mut SHMS[index];
        if !s.used || s.refs == 0 {
            asm!("csrsi sstatus, 2");
            return;
        }
        // 清掉该进程的映射记录（页表已销毁，无需 unmap），并按清除的
        // 映射数归还引用（与 shm_map 的逐映射 +1 对账）。
        let mut maps_released = 0usize;
        for m in s.maps.iter_mut() {
            if m.0 == pid {
                m.0 = 0;
                m.1 = 0;
                maps_released += 1;
            }
        }
        // fd 引用 -1（本 fd 本身），再按映射数归还
        let total = 1usize.saturating_add(maps_released);
        s.refs = s.refs.saturating_sub(total as u32);
        if s.refs == 0 {
            crate::allocator::free_page(s.page);
            *s = Shm::empty();
        }
    }
    unsafe { asm!("csrsi sstatus, 2") };
}

/// 增加管道引用计数（fork/dup 复制 fd 时调用，防止子进程退出清空父进程管道）。
/// `is_write` 区分该 fd 是写端还是读端：EOF/EPIPE 判定按「对端 fd 是否全部
/// 关闭」进行，而不只是总引用归零——只关写端也必须唤醒等 EOF 的读者。
pub fn pipe_add_ref(index: usize, is_write: bool) {
    if index < MAX_PIPES {
        // 引用计数为普通读-改-写：调用点（fork/dup）处于 IRQ 使能的任务
        // 上下文，被 tick 抢占后另一任务对同一管道并发 add/drop 会丢更新
        // → 槽位泄漏或提前清空。与 pipe_write 同款屏蔽。
        unsafe { asm!("csrci sstatus, 2") };
        unsafe {
            if PIPES[index].used {
                PIPES[index].refs = PIPES[index].refs.saturating_add(1);
                if is_write {
                    PIPES[index].writers = PIPES[index].writers.saturating_add(1);
                } else {
                    PIPES[index].readers = PIPES[index].readers.saturating_add(1);
                }
            }
        }
        unsafe { asm!("csrsi sstatus, 2") };
    }
}

/// 释放一个管道引用（close/exit 时调用）。`is_write` 标记该 fd 端。
///
/// 三方语义：
/// - 最后一个**写端**关闭（`writers` 归零而读者仍在）→ 唤醒全部读者，
///   读者醒来发现 `writers == 0` 按 EOF（0 字节）返回（此前只关写端不
///   唤醒，读空的读者永久挂起）；槽位在 `refs` 归零后仍会清空。
/// - 最后一个**读端**关闭 → 唤醒全部写者，写者返回 EPIPE（写计数停止）。
/// - `refs` 归零（所有 fd 均已关闭）→ 清空槽位（两侧等候者同时唤醒）。
pub fn pipe_drop_ref(index: usize, is_write: bool) {
    if index >= MAX_PIPES {
        return;
    }
    // 引用计数/端计数为普通读-改-写，且尾部要唤醒等待者：全程屏蔽 IRQ
    // （fork 回滚、reap、close 收尾都在 IRQ 使能上下文，并发 drop 丢一次
    // 递减 → 槽位与其中的等待者永久泄漏）。
    unsafe { asm!("csrci sstatus, 2") };
    unsafe {
        let p = &mut PIPES[index];
        if !p.used {
            asm!("csrsi sstatus, 2");
            return;
        }
        if p.refs > 0 {
            p.refs -= 1;
        }
        if is_write {
            if p.writers > 0 {
                p.writers -= 1;
            }
        } else {
            if p.readers > 0 {
                p.readers -= 1;
            }
        }
        if p.writers == 0 {
            wait_wake(&mut p.reader_wait);
        }
        if p.readers == 0 {
            wait_wake(&mut p.writer_wait);
        }
        if p.refs == 0 {
            wait_wake(&mut p.reader_wait);
            wait_wake(&mut p.writer_wait);
            *p = Pipe::empty();
        }
    }
    unsafe { asm!("csrsi sstatus, 2") };
}

/// 增加消息队列引用计数（fork/dup 复制 fd 时调用）。
pub fn msgq_add_ref(index: usize) {
    if index < MAX_QUEUES {
        unsafe { asm!("csrci sstatus, 2") };
        unsafe {
            if QUEUES[index].used {
                QUEUES[index].refs = QUEUES[index].refs.saturating_add(1);
            }
        }
        unsafe { asm!("csrsi sstatus, 2") };
    }
}

/// 释放一个消息队列引用（close/exit 时调用），引用归零才清空槽位。
pub fn msgq_drop_ref(index: usize) {
    if index >= MAX_QUEUES {
        return;
    }
    unsafe { asm!("csrci sstatus, 2") };
    unsafe {
        let q = &mut QUEUES[index];
        if !q.used {
            asm!("csrsi sstatus, 2");
            return;
        }
        if q.refs > 0 {
            q.refs -= 1;
        }
        if q.refs == 0 {
            // ★ 最后一方 close：唤醒阻塞中的收发线程（此前只清槽位不唤醒——
            // 读空等消息的接收者 / 写满等槽位的发送者会永久挂起）。清空前
            // 唤醒：接收者醒来见 used==false 返回 EOF，发送者返回错误。
            wait_wake(&mut q.recv_wait);
            wait_wake(&mut q.send_wait);
            *q = MsgQueue::empty();
        }
    }
    unsafe { asm!("csrsi sstatus, 2") };
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
    // ★ 同 shm_map：refs 读-改-写与并发 map/add_ref 的交错会丢更新 → 提前
    // 归零释放仍被映射的物理页。全程屏蔽 IRQ。
    unsafe { asm!("csrci sstatus, 2") };
    unsafe {
        let s = &mut SHMS[entry.index];
        if !s.used || s.refs == 0 {
            unsafe { asm!("csrsi sstatus, 2") };
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
    unsafe { asm!("csrsi sstatus, 2") };
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
    /// 引用 side0 的 fd 数（写 ring_a / 读 ring_b）。
    side0: u32,
    /// 引用 side1 的 fd 数（写 ring_b / 读 ring_a）。
    side1: u32,
}

impl PairSock {
    const fn empty() -> Self {
        PairSock {
            used: false,
            ring_a: PairRing::empty(),
            ring_b: PairRing::empty(),
            refs: 0,
            side0: 0,
            side1: 0,
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
        PAIRS[idx].side0 = 1;
        PAIRS[idx].side1 = 1;
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

/// 取 side 的写环（对方读）：side0 写 ring_a，side1 写 ring_b。
///
/// 每次访问都从 `PAIRS[idx]` 重新取引用——阻塞唤醒后槽位可能已被
/// `pair_drop_ref` 清空并被新 `socketpair_create` 复用，跨阻塞持有裸指针
/// 会把数据写进新连接的环（跨连接串扰）。
fn pair_write_ring(idx: usize, side: usize) -> &'static mut PairRing {
    unsafe {
        if side == 0 {
            &mut PAIRS[idx].ring_a
        } else {
            &mut PAIRS[idx].ring_b
        }
    }
}

/// 取 side 的读环（对方写）：side0 读 ring_b，side1 读 ring_a。
fn pair_read_ring(idx: usize, side: usize) -> &'static mut PairRing {
    unsafe {
        if side == 0 {
            &mut PAIRS[idx].ring_b
        } else {
            &mut PAIRS[idx].ring_a
        }
    }
}

/// 对端 side 的 fd 计数：side0 的对端是 side1，反之亦然。
fn pair_peer_count(idx: usize, side: usize) -> u32 {
    unsafe {
        if side == 0 {
            PAIRS[idx].side1
        } else {
            PAIRS[idx].side0
        }
    }
}

/// `socketpair` fd 写入：写入本方写环（对方读）；写满时阻塞直至写完。
///
/// 与 [`pipe_write`] 同款临界区纪律：全程屏蔽 IRQ；每轮循环重查槽位
/// `used` 与对端 fd 计数——对端全部关闭（EPIPE 语义）或槽位被清空复用
/// 时立即返回已写字节数，不再等待（此前永久挂起或写进复用后的新连接）。
pub fn pair_write(fd: i32, data: &[u8]) -> usize {
    let entry = match process::get_fd(fd) {
        Some(e) if e.kind == FdKind::Pair => e,
        _ => return 0,
    };
    if entry.index >= MAX_PAIRS {
        return 0;
    }
    let idx = entry.index;
    let side = entry.flags as usize;
    let mut written = 0usize;
    unsafe { asm!("csrci sstatus, 2") };
    while written < data.len() {
        // 槽位已清空或对端 fd 全部关闭：停止写入（短写 = EPIPE 语义）
        if !unsafe { PAIRS[idx].used } || pair_peer_count(idx, side) == 0 {
            break;
        }
        let count = pair_write_ring(idx, side).count;
        let n = (data.len() - written).min(PAIR_BUF - count);
        if n == 0 {
            // 缓冲满：登记写等待并阻塞（屏蔽窗口贯穿登记→阻塞，不丢唤醒）
            if !wait_reg(&mut pair_write_ring(idx, side).write_wait, sched_cfs::current_tid()) {
                // 等待者表满：让出 CPU 后重试登记
                unsafe { asm!("csrsi sstatus, 2") };
                sched_cfs::yield_now();
                unsafe { asm!("csrci sstatus, 2") };
                continue;
            }
            sched_cfs::block_current();
            unsafe { asm!("csrci sstatus, 2") };
            continue;
        }
        {
            let ring = pair_write_ring(idx, side);
            for i in 0..n {
                ring.buf[(ring.tail + i) % PAIR_BUF] = data[written + i];
            }
            ring.tail = (ring.tail + n) % PAIR_BUF;
            ring.count += n;
        }
        written += n;
        wait_wake(&mut pair_read_ring(idx, side).read_wait);
    }
    unsafe { asm!("csrsi sstatus, 2") };
    written
}

/// `socketpair` fd 读取：从本方读环（对方写）取数据；空时阻塞。
///
/// 对端 fd 全部关闭或槽位清空且无数据 → 返回 0（EOF 语义，此前对端
/// 关闭只减引用不唤醒，阻塞读者永久挂起）。
pub fn pair_read(fd: i32, out: &mut [u8]) -> usize {
    let entry = match process::get_fd(fd) {
        Some(e) if e.kind == FdKind::Pair => e,
        _ => return 0,
    };
    if entry.index >= MAX_PAIRS || out.is_empty() {
        return 0;
    }
    let idx = entry.index;
    let side = entry.flags as usize;
    unsafe { asm!("csrci sstatus, 2") };
    // 空缓冲且对端仍在：登记读等待并阻塞
    while unsafe { PAIRS[idx].used && pair_peer_count(idx, side) > 0 && pair_read_ring(idx, side).count == 0 } {
        if !wait_reg(&mut pair_read_ring(idx, side).read_wait, sched_cfs::current_tid()) {
            // 等待者表满：让出 CPU 后重试登记
            unsafe { asm!("csrsi sstatus, 2") };
            sched_cfs::yield_now();
            unsafe { asm!("csrci sstatus, 2") };
            continue;
        }
        sched_cfs::block_current();
        unsafe { asm!("csrci sstatus, 2") };
    }
    // EOF：槽位已清空或对端 fd 全部关闭且无数据
    if !unsafe { PAIRS[idx].used } {
        unsafe { asm!("csrsi sstatus, 2") };
        return 0;
    }
    let n = {
        let ring = pair_read_ring(idx, side);
        let n = out.len().min(ring.count);
        for i in 0..n {
            out[i] = ring.buf[(ring.head + i) % PAIR_BUF];
        }
        ring.head = (ring.head + n) % PAIR_BUF;
        ring.count -= n;
        n
    };
    wait_wake(&mut pair_write_ring(idx, side).write_wait);
    unsafe { asm!("csrsi sstatus, 2") };
    n
}

/// 增加套接字对引用计数（fork/dup 复制 fd 时调用，防子进程 close/exit
/// 提前清空对端槽位 —— 与管道/消息队列同源的多进程生命周期问题）。
/// `side` 为该 fd 的端（0/1），EOF/EPIPE 判定按「对端 fd 是否全部关闭」。
pub fn pair_add_ref(index: usize, side: u32) {
    if index >= MAX_PAIRS {
        return;
    }
    unsafe { asm!("csrci sstatus, 2") };
    unsafe {
        if PAIRS[index].used {
            PAIRS[index].refs = PAIRS[index].refs.saturating_add(1);
            if side == 0 {
                PAIRS[index].side0 = PAIRS[index].side0.saturating_add(1);
            } else {
                PAIRS[index].side1 = PAIRS[index].side1.saturating_add(1);
            }
        }
    }
    unsafe { asm!("csrsi sstatus, 2") };
}

/// 按下标释放一个套接字对引用（close/exit 时调用），引用归零才清空槽位。
///
/// 与 [`pipe_drop_ref`] 一致：一侧 fd 全部关闭时唤醒对端阻塞的读写线程
/// （读者见 EOF、写者短写退出，此前只在 refs 归零时唤醒——对端关闭后
/// 阻塞在空环/满环上的读写线程会永久挂起）。
pub fn pair_drop_ref(index: usize, side: u32) {
    if index >= MAX_PAIRS {
        return;
    }
    unsafe { asm!("csrci sstatus, 2") };
    unsafe {
        let p = &mut PAIRS[index];
        if !p.used {
            asm!("csrsi sstatus, 2");
            return;
        }
        if p.refs > 0 {
            p.refs -= 1;
        }
        if side == 0 {
            if p.side0 > 0 {
                p.side0 -= 1;
            }
        } else {
            if p.side1 > 0 {
                p.side1 -= 1;
            }
        }
        if p.refs == 0 {
            wait_wake(&mut p.ring_a.read_wait);
            wait_wake(&mut p.ring_a.write_wait);
            wait_wake(&mut p.ring_b.read_wait);
            wait_wake(&mut p.ring_b.write_wait);
            *p = PairSock::empty();
        } else if p.side0 == 0 || p.side1 == 0 {
            // 某一侧 fd 全关：唤醒两侧等待者（对端读者醒来见 EOF/槽位判定，
            // 对端写者醒来见 peer count == 0 短写退出）
            wait_wake(&mut p.ring_a.read_wait);
            wait_wake(&mut p.ring_a.write_wait);
            wait_wake(&mut p.ring_b.read_wait);
            wait_wake(&mut p.ring_b.write_wait);
        }
    }
    unsafe { asm!("csrsi sstatus, 2") };
}

/// `socketpair` fd 关闭：引用计数 -1，归零释放槽位。
pub fn pair_close(fd: i32) {
    if let Some(entry) = process::get_fd(fd) {
        if entry.kind == FdKind::Pair {
            pair_drop_ref(entry.index, entry.flags as u32);
        }
    }
}
