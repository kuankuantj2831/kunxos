//! socket 接口：套接字句柄表 + UDP/TCP 端点。
//!
//! 提供类 POSIX 的简化接口：`socket_udp`/`socket_tcp` 创建，`bind` 绑定本地端口，
//! `connect` 建立连接（TCP 三次握手 / UDP 记录对端），`send`/`send_to` 发送，
//! `recv`/`recv_from` 接收，`close` 关闭。
//!
//! TCP 实现为客户端主动连接：SYN → SYN-ACK → ACK；数据段带 ACK 确认与简单
//! 重传（超时按轮询轮数计）；收到的数据按序入接收缓冲并回 ACK；FIN 做简化
//! 四次挥手。阻塞等待采用「轮询 + 让出 CPU」而非永久阻塞，从而能同时驱动
//! 重传计时与超时。

use crate::net_stack;
use crate::sched_cfs;

// ==================== 常量 ====================

const SOCK_UDP: u8 = 0;
const SOCK_TCP: u8 = 1;

const MAX_SOCKETS: usize = 16;
const MAX_UDP: usize = 8;
const MAX_TCP: usize = 8;

// TCP 标志
const TCP_FIN: u8 = 0x01;
const TCP_SYN: u8 = 0x02;
#[allow(dead_code)]
const TCP_RST: u8 = 0x04;
const TCP_PSH: u8 = 0x08;
const TCP_ACK: u8 = 0x10;

// TCP 状态
const T_CLOSED: u8 = 0;
const T_SYN_SENT: u8 = 1;
const T_ESTABLISHED: u8 = 2;
const T_FIN_WAIT1: u8 = 3;
const T_FIN_WAIT2: u8 = 4;
const T_CLOSE_WAIT: u8 = 5;
const T_LAST_ACK: u8 = 6;
const T_TIME_WAIT: u8 = 7;
/// 监听（accept 等待连接）。
const T_LISTEN: u8 = 8;
/// 服务器侧已收到 SYN、发出 SYN-ACK，等待客户端 ACK。
const T_SYN_RCVD: u8 = 9;

const TCP_WINDOW: u16 = 16384;
const TCP_RXBUF: usize = 16384;
const TCP_RTXBUF: usize = 2048;

/// 重传间隔与各阶段超时（微秒，基于虚拟计数器）。
const RETX_US: u64 = 1_000_000;
const CONNECT_TIMEOUT_US: u64 = 30_000_000;
const SEND_TIMEOUT_US: u64 = 30_000_000;
const RECV_TIMEOUT_US: u64 = 60_000_000;
const CLOSE_TIMEOUT_US: u64 = 10_000_000;
const UDP_TIMEOUT_US: u64 = 30_000_000;
/// 服务器侧 T_SYN_RCVD 子连接最长等待时间（客户端不回 ACK 则回收槽位）。
const SYN_RCVD_TIMEOUT_US: u64 = 30_000_000;
/// TIME_WAIT 最长滞留时间（简化 2MSL）：超时自动回收槽位，此前只能靠
/// 用户 close 解除 —— 忘记 close 的连接永久占用 TCP 槽位。
const TIME_WAIT_TIMEOUT_US: u64 = 30_000_000;

const UDP_QUEUE: usize = 8;
const UDP_MSZ: usize = 1472;

// ==================== 套接字句柄表 ====================

#[derive(Clone, Copy)]
struct Socket {
    used: bool,
    ty: u8,
    index: usize,
}

const EMPTY_SOCKET: Socket = Socket { used: false, ty: 0, index: 0 };
static mut SOCKETS: [Socket; MAX_SOCKETS] = [EMPTY_SOCKET; MAX_SOCKETS];

// ==================== UDP 端点 ====================

#[derive(Clone, Copy)]
struct UdpDatagram {
    used: bool,
    ip: u32,
    port: u16,
    data: [u8; UDP_MSZ],
    len: usize,
}

const EMPTY_DGRAM: UdpDatagram =
    UdpDatagram { used: false, ip: 0, port: 0, data: [0; UDP_MSZ], len: 0 };

struct UdpSock {
    used: bool,
    local_port: u16,
    remote_ip: u32,
    remote_port: u16,
    rx: [UdpDatagram; UDP_QUEUE],
    rx_head: usize,
    rx_count: usize,
}

impl UdpSock {
    const fn empty() -> Self {
        UdpSock {
            used: false,
            local_port: 0,
            remote_ip: 0,
            remote_port: 0,
            rx: [EMPTY_DGRAM; UDP_QUEUE],
            rx_head: 0,
            rx_count: 0,
        }
    }
}

static mut UDPS: [UdpSock; MAX_UDP] = [const { UdpSock::empty() }; MAX_UDP];

// ==================== TCP 端点 ====================

struct TcpSock {
    used: bool,
    state: u8,
    local_ip: u32,
    local_port: u16,
    remote_ip: u32,
    remote_port: u16,
    iss: u32,
    snd_nxt: u32,
    snd_una: u32,
    rcv_nxt: u32,
    rx: [u8; TCP_RXBUF],
    rx_head: usize,
    rx_count: usize,
    rtx: [u8; TCP_RTXBUF],
    rtx_len: usize,
    rtx_seq: u32,
    rtx_active: bool,
    fin_sent: bool,
    /// close 时数据段重传仍在登记（send 超时未获 ACK）：FIN 暂不能占用重传
    /// 槽位，待数据被确认后补登（fin_rtx_pending → 重传槽位换 FIN）。
    fin_rtx_pending: bool,
    peer_fin: bool,
    /// 监听套接字的待 accept 连接（子 TCP 槽位下标）。
    pending: [usize; MAX_TCP],
    pending_count: usize,
    // ===== 拥塞控制与重传（Reno 简化）=====
    /// 拥塞窗口（字节）。
    cwnd: u32,
    /// 慢启动阈值（字节）。
    ssthresh: u32,
    /// 当前重传超时（微秒，随 RTT 估计与退避调整）。
    rto_us: u64,
    /// 最近一次发送时间戳（RTT 采样）。
    last_send_us: u64,
    /// 连续重传次数（指数退避）。
    retx_cnt: u32,
}

impl TcpSock {
    const fn empty() -> Self {
        TcpSock {
            used: false,
            state: T_CLOSED,
            local_ip: 0,
            local_port: 0,
            remote_ip: 0,
            remote_port: 0,
            iss: 0,
            snd_nxt: 0,
            snd_una: 0,
            rcv_nxt: 0,
            rx: [0; TCP_RXBUF],
            rx_head: 0,
            rx_count: 0,
            rtx: [0; TCP_RTXBUF],
            rtx_len: 0,
            rtx_seq: 0,
            rtx_active: false,
            fin_sent: false,
            fin_rtx_pending: false,
            peer_fin: false,
            pending: [usize::MAX; MAX_TCP],
            pending_count: 0,
            cwnd: 4 * 1024, // 初始 4 段
            ssthresh: 64 * 1024,
            rto_us: RETX_US,
            last_send_us: 0,
            retx_cnt: 0,
        }
    }
}

static mut TCPS: [TcpSock; MAX_TCP] = [const { TcpSock::empty() }; MAX_TCP];

/// 发送段组装缓冲（经 net_stack::net_lock 与其他任务互斥）。
static mut TCPBUF: [u8; 2048] = [0; 2048];

/// 初始序号计数器。
static mut ISS_COUNTER: u32 = 0x2000_0000;

// 并发约定：syscall 上下文 IRQ 使能、可被 tick 抢占切任务，本文件全部静态
// 状态（SOCKETS/UDPS/TCPS/TCPBUF/ISS_COUNTER）由 net_stack::net_lock 保护。
// 短操作整体持锁；阻塞等待循环（accept/connect/send/recv/close）不得持锁
// 跨过 poll/yield——只包住状态读写与重传段。tcp_input/udp_input 由
// net_stack::poll（持锁）分发到达；tcp_xmit/tcp_retransmit/tcp_rx_push/
// tcp_rx_drain/find_tcp/tcp_reap_syn_rcvd 的全部调用点均处于持锁上下文。
fn next_iss() -> u32 {
    net_stack::net_lock();
    let r = unsafe {
        ISS_COUNTER = ISS_COUNTER.wrapping_add(0x1_0000);
        ISS_COUNTER
    };
    net_stack::net_unlock();
    r
}

// ==================== 初始化 ====================

/// 复位套接字 / 端点表。
pub fn init() {
    unsafe {
        for s in SOCKETS.iter_mut() {
            *s = EMPTY_SOCKET;
        }
        for u in UDPS.iter_mut() {
            *u = UdpSock::empty();
        }
        for t in TCPS.iter_mut() {
            *t = TcpSock::empty();
        }
    }
}

// ==================== 句柄管理 ====================

fn alloc_socket(ty: u8, index: usize) -> i32 {
    unsafe {
        for i in 0..MAX_SOCKETS {
            if !SOCKETS[i].used {
                SOCKETS[i] = Socket { used: true, ty, index };
                return i as i32;
            }
        }
    }
    -1
}

fn socket_index(h: i32) -> Option<Socket> {
    if h < 0 || (h as usize) >= MAX_SOCKETS {
        return None;
    }
    unsafe {
        let s = SOCKETS[h as usize];
        if s.used {
            Some(s)
        } else {
            None
        }
    }
}

/// 判断句柄是否为有效的已打开套接字（供 `close` 等系统调用区分
/// 「未登记的 fd」与「socket 句柄」）。
pub fn is_socket(h: i32) -> bool {
    socket_index(h).is_some()
}

/// Snapshot poll readiness under the same lock as send/recv and packet input.
pub fn poll_events(h: i32) -> Option<i16> {
    net_stack::net_lock();
    let result = socket_index(h).map(|s| unsafe {
        if s.ty == SOCK_UDP {
            0x004 | if UDPS[s.index].rx_count > 0 { 0x001 } else { 0 }
        } else {
            let t = &TCPS[s.index];
            if t.state == T_LISTEN {
                if t.pending_count > 0 { 0x001 } else { 0 }
            } else {
                let mut events = 0;
                if t.rx_count > 0 || t.peer_fin || t.state == T_CLOSED { events |= 0x001; }
                if (t.state == T_ESTABLISHED || t.state == T_CLOSE_WAIT) && !t.rtx_active { events |= 0x004; }
                if t.state == T_CLOSED { events |= 0x010; }
                events
            }
        }
    });
    net_stack::net_unlock();
    result
}

// ==================== 公共接口 ====================

/// 端口是否已被任一 UDP/TCP 套接字绑定（临时端口自动分配时跳过冲突）。
pub fn port_in_use(port: u16) -> bool {
    net_stack::net_lock();
    let r = unsafe {
        let mut used = false;
        for u in UDPS.iter() {
            if u.used && u.local_port == port {
                used = true;
                break;
            }
        }
        if !used {
            for t in TCPS.iter() {
                if t.used && t.local_port == port {
                    used = true;
                    break;
                }
            }
        }
        used
    };
    net_stack::net_unlock();
    r
}

/// 创建 UDP 套接字，返回句柄。
#[allow(dead_code)]
pub fn socket_udp() -> i32 {
    net_stack::net_lock();
    let r = unsafe {
        let mut h = -1;
        for i in 0..MAX_UDP {
            if !UDPS[i].used {
                UDPS[i].used = true;
                h = alloc_socket(SOCK_UDP, i);
                break;
            }
        }
        h
    };
    net_stack::net_unlock();
    r
}

/// 创建 TCP 套接字，返回句柄。
pub fn socket_tcp() -> i32 {
    net_stack::net_lock();
    let r = unsafe {
        let mut h = -1;
        for i in 0..MAX_TCP {
            if !TCPS[i].used {
                TCPS[i] = TcpSock::empty();
                TCPS[i].used = true;
                TCPS[i].local_ip = net_stack::local_ip();
                h = alloc_socket(SOCK_TCP, i);
                break;
            }
        }
        h
    };
    net_stack::net_unlock();
    r
}

/// 绑定本地端口。
///
/// `port == 0` 表示由内核自动分配临时端口（在首次 connect/send 时完成）；
/// 非 0 端口若已被同类型其它套接字占用则返回 -1（对应 Linux `EADDRINUSE`）。
#[allow(dead_code)]
pub fn bind(h: i32, port: u16) -> i32 {
    let Some(s) = socket_index(h) else { return -1 };
    if port == 0 {
        return 0; // 延迟到 connect/send 时自动分配
    }
    net_stack::net_lock();
    let r = unsafe {
        match s.ty {
            SOCK_UDP => {
                let mut ok = true;
                for i in 0..MAX_UDP {
                    if i != s.index && UDPS[i].used && UDPS[i].local_port == port {
                        ok = false; // 端口已被占用
                        break;
                    }
                }
                if ok {
                    UDPS[s.index].local_port = port;
                    0
                } else {
                    -1
                }
            }
            SOCK_TCP => {
                let mut ok = true;
                for i in 0..MAX_TCP {
                    if i != s.index && TCPS[i].used && TCPS[i].local_port == port {
                        ok = false;
                        break;
                    }
                }
                if ok {
                    TCPS[s.index].local_port = port;
                    0
                } else {
                    -1
                }
            }
            _ => -1,
        }
    };
    net_stack::net_unlock();
    r
}

/// 监听：把已 bind 的 TCP 套接字置为监听状态，开始接受连接。
/// 返回 0 或负 errno（此前一律 -1=EPERM，语义偏差）。
pub fn listen(h: i32, backlog: usize) -> i32 {
    let Some(s) = socket_index(h) else { return -9 }; // -EBADF
    if s.ty != SOCK_TCP {
        return -22; // -EINVAL
    }
    let _ = backlog; // 简化：待 accept 队列容量 = MAX_TCP
    net_stack::net_lock();
    let r = unsafe {
        let t = &mut TCPS[s.index];
        if t.local_port == 0 {
            net_stack::net_unlock();
            return -22; // -EINVAL：必须先 bind
        }
        t.state = T_LISTEN;
        0
    };
    net_stack::net_unlock();
    r
}

/// 接受一个连接（阻塞直到有客户端接入），返回新的已连接套接字句柄。
/// 失败返回负 errno（此前一律 -1=EPERM）。
pub fn accept(h: i32) -> i32 {
    let Some(s) = socket_index(h) else { return -9 }; // -EBADF
    if s.ty != SOCK_TCP {
        return -22; // -EINVAL
    }
    let lsn = s.index;
    'outer: loop {
        // 轮询驱动握手（poll 自身持锁），同时回收过期的 T_SYN_RCVD 子连接
        net_stack::poll();
        net_stack::net_lock();
        unsafe { tcp_reap_syn_rcvd() };
        // 取最旧的待 accept 子连接
        let child = unsafe {
            let l = &mut TCPS[lsn];
            if l.state != T_LISTEN {
                net_stack::net_unlock();
                return -22; // -EINVAL：未处于监听态
            }
            if l.pending_count > 0 {
                let c = l.pending[0];
                // 前移队列
                for i in 1..l.pending_count {
                    l.pending[i - 1] = l.pending[i];
                }
                l.pending_count -= 1;
                l.pending[l.pending_count] = usize::MAX;
                Some(c)
            } else {
                None
            }
        };
        net_stack::net_unlock();
        if let Some(c) = child {
            // 子连接可能尚在 T_SYN_RCVD（SYN-ACK 刚发出、客户端 ACK 未到）：
            // 此前直接返回 fd，调用方在握手完成前就收发会丢段。这里先轮询
            // 等到 T_ESTABLISHED 再返回；失败（RST/超时）则回收子槽位继续等。
            let start = crate::timer::now_us();
            loop {
                net_stack::net_lock();
                let st = unsafe { TCPS[c].state };
                if st == T_ESTABLISHED {
                    net_stack::net_unlock();
                    break;
                }
                if st == T_CLOSED || crate::timer::now_us() - start > CONNECT_TIMEOUT_US {
                    unsafe { TCPS[c] = TcpSock::empty(); }
                    net_stack::net_unlock();
                    continue 'outer;
                }
                net_stack::net_unlock();
                net_stack::poll();
                sched_cfs::yield_now();
            }
            // 为子连接分配句柄
            net_stack::net_lock();
            let fd = alloc_socket(SOCK_TCP, c);
            net_stack::net_unlock();
            if fd >= 0 {
                return fd;
            }
            // ★ 句柄耗尽：在锁内清子连接槽位（解锁后清会与其它任务的
            // poll 投递竞态 —— 数据/ACK 写入后被整体清零，段丢失/状态撕裂），
            // 否则 ESTABLISHED 子连接无句柄持有、reap（只回收 SYN_RCVD）
            // 也不回收 → TCP 槽位永久泄漏，服务器拒绝服务。
            net_stack::net_lock();
            unsafe { TCPS[c] = TcpSock::empty(); }
            net_stack::net_unlock();
            return -12; // -ENOMEM：套接字句柄耗尽
        }
        // 无待 accept 连接：轮询等待
        sched_cfs::yield_now();
    }
}

/// 建立连接：UDP 记录对端；TCP 执行三次握手。
pub fn connect(h: i32, ip: u32, port: u16) -> i32 {
    let Some(s) = socket_index(h) else { return -1 };
    match s.ty {
        SOCK_UDP => {
            // 目标端口 0 非法：内核不向 dport 0 投递到未绑定套接字
            if port == 0 {
                return -22; // -EINVAL
            }
            net_stack::net_lock();
            let r = unsafe {
                UDPS[s.index].remote_ip = ip;
                UDPS[s.index].remote_port = port;
                if UDPS[s.index].local_port == 0 {
                    UDPS[s.index].local_port = net_stack::next_port();
                }
                0
            };
            net_stack::net_unlock();
            r
        }
        SOCK_TCP => tcp_connect(s.index, ip, port),
        _ => -1,
    }
}

/// 发送数据（TCP：面向连接；UDP：发往 connect 设定的对端）。
pub fn send(h: i32, data: &[u8]) -> i32 {
    let Some(s) = socket_index(h) else { return -1 };
    match s.ty {
        SOCK_UDP => unsafe { udp_send_connected(s.index, data) },
        SOCK_TCP => tcp_send(s.index, data),
        _ => -1,
    }
}

/// UDP 发送到指定地址。
#[allow(dead_code)]
pub fn send_to(h: i32, data: &[u8], ip: u32, port: u16) -> i32 {
    let Some(s) = socket_index(h) else { return -1 };
    if s.ty != SOCK_UDP {
        return -1;
    }
    // 目标端口 0 非法：内核不向 dport 0 投递到未绑定套接字
    if port == 0 {
        return -22; // -EINVAL
    }
    unsafe { udp_send_raw(s.index, ip, port, data) }
}

/// 接收数据（阻塞）。返回字节数，<=0 表示失败或对端关闭。
pub fn recv(h: i32, buf: &mut [u8]) -> i32 {
    let Some(s) = socket_index(h) else { return -1 };
    match s.ty {
        SOCK_UDP => unsafe { udp_recv(s.index, buf) },
        SOCK_TCP => tcp_recv(s.index, buf),
        _ => -1,
    }
}

/// UDP 接收，同时返回源地址。
#[allow(dead_code)]
pub fn recv_from(h: i32, buf: &mut [u8]) -> (i32, u32, u16) {
    let Some(s) = socket_index(h) else { return (-1, 0, 0) };
    if s.ty != SOCK_UDP {
        return (-1, 0, 0);
    }
    unsafe { udp_recv_from(s.index, buf) }
}

/// 遍历已打开套接字，对每个调用 `f(handle, type, state, local_port, remote_ip, remote_port)`。
///
/// 供 `netstat` 类工具显示连接状态。
pub fn for_each_socket(f: &mut dyn FnMut(i32, &str, &str, u16, u32, u16)) {
    net_stack::net_lock();
    unsafe {
        for h in 0..MAX_SOCKETS {
            let s = SOCKETS[h];
            if !s.used {
                continue;
            }
            match s.ty {
                SOCK_UDP => {
                    let u = &UDPS[s.index];
                    let state = if u.local_port != 0 { "UNCONN" } else { "CLOSED" };
                    f(h as i32, "UDP", state, u.local_port, u.remote_ip, u.remote_port);
                }
                SOCK_TCP => {
                    let t = &TCPS[s.index];
                    let st = match t.state {
                        T_SYN_SENT => "SYN_SENT",
                        T_ESTABLISHED => "ESTAB",
                        T_FIN_WAIT1 => "FIN_WAIT1",
                        T_FIN_WAIT2 => "FIN_WAIT2",
                        T_CLOSE_WAIT => "CLOSE_WAIT",
                        T_LAST_ACK => "LAST_ACK",
                        T_TIME_WAIT => "TIME_WAIT",
                        T_LISTEN => "LISTEN",
                        T_SYN_RCVD => "SYN_RCVD",
                        _ => "CLOSED",
                    };
                    f(h as i32, "TCP", st, t.local_port, t.remote_ip, t.remote_port);
                }
                _ => {}
            }
        }
    }
    net_stack::net_unlock();
}

/// 关闭套接字。
pub fn close(h: i32) {
    let Some(s) = socket_index(h) else { return };
    match s.ty {
        SOCK_TCP => {
            net_stack::net_lock();
            unsafe {
                // 监听套接字：释放全部待 accept 子连接
                let l = &TCPS[s.index];
                if l.state == T_LISTEN {
                    for &c in l.pending.iter() {
                        if c < MAX_TCP && TCPS[c].used {
                            TCPS[c] = TcpSock::empty();
                        }
                    }
                }
            }
            net_stack::net_unlock();
            tcp_close(s.index);
        }
        _ => {
            net_stack::net_lock();
            unsafe {
                UDPS[s.index] = UdpSock::empty();
                SOCKETS[h as usize] = EMPTY_SOCKET;
            }
            net_stack::net_unlock();
        }
    }
}

// ==================== UDP 实现 ====================

unsafe fn udp_send_raw(idx: usize, ip: u32, port: u16, data: &[u8]) -> i32 {
    net_stack::net_lock();
    // 未绑定/未 connect 过（本地端口为 0）：自动分配临时端口（Linux sendto 语义）
    if UDPS[idx].local_port == 0 {
        UDPS[idx].local_port = net_stack::next_port();
    }
    let sport = UDPS[idx].local_port;
    let r = net_stack::udp_send(ip, sport, port, data);
    net_stack::net_unlock();
    r
}

unsafe fn udp_send_connected(idx: usize, data: &[u8]) -> i32 {
    let ip = UDPS[idx].remote_ip;
    let port = UDPS[idx].remote_port;
    if ip == 0 {
        return -1;
    }
    udp_send_raw(idx, ip, port, data)
}

/// 网卡收到的 UDP 包按目标端口分发给绑定套接字。
pub fn udp_input(src_ip: u32, sport: u16, dport: u16, data: &[u8]) {
    unsafe {
        for i in 0..MAX_UDP {
            let u = &mut UDPS[i];
            // 未绑定套接字（local_port == 0）不接收任何包，防止 dport==0
            // 的伪造/损坏包被投递到未 bind 的套接字
            if !u.used || u.local_port == 0 || u.local_port != dport {
                continue;
            }
            // 已 connect 的套接字只接受对端数据
            if u.remote_ip != 0 && (u.remote_ip != src_ip || u.remote_port != sport) {
                continue;
            }
            if u.rx_count >= UDP_QUEUE {
                return;
            }
            let slot = (u.rx_head + u.rx_count) % UDP_QUEUE;
            let n = data.len().min(UDP_MSZ);
            u.rx[slot].used = true;
            u.rx[slot].ip = src_ip;
            u.rx[slot].port = sport;
            u.rx[slot].data[..n].copy_from_slice(&data[..n]);
            u.rx[slot].len = n;
            u.rx_count += 1;
            return;
        }
    }
}

unsafe fn udp_recv_from(idx: usize, buf: &mut [u8]) -> (i32, u32, u16) {
    let start = crate::timer::now_us();
    loop {
        // 出队段持锁；poll/yield 不持锁（否则阻塞中的读者会卡住其他任务
        // 的全部联网操作直至超时）
        net_stack::net_lock();
        let out = {
            let u = &mut UDPS[idx];
            if u.rx_count > 0 {
                let slot = u.rx_head;
                let dg = u.rx[slot];
                let n = dg.len.min(buf.len());
                buf[..n].copy_from_slice(&dg.data[..n]);
                u.rx[slot].used = false;
                u.rx_head = (u.rx_head + 1) % UDP_QUEUE;
                u.rx_count -= 1;
                Some((n as i32, dg.ip, dg.port))
            } else {
                None
            }
        };
        net_stack::net_unlock();
        if let Some(r) = out {
            return r;
        }
        net_stack::poll();
        if crate::timer::now_us() - start > UDP_TIMEOUT_US {
            return (-1, 0, 0);
        }
        sched_cfs::yield_now();
    }
}

unsafe fn udp_recv(idx: usize, buf: &mut [u8]) -> i32 {
    let (n, _, _) = udp_recv_from(idx, buf);
    n
}

// ==================== TCP 实现 ====================

/// 发送一个 TCP 段（seq 由参数指定，ack 取当前 rcv_nxt）。
/// 返回 `false` = 段未上线路（超长或 IP 层发送失败）——调用方**不得**
/// 推进 snd_nxt（此前静默丢段但 snd_nxt 照常推进 → 连接永久失步）。
unsafe fn tcp_xmit(idx: usize, seq: u32, flags: u8, data: &[u8]) -> bool {
    let (local_ip, remote_ip, local_port, remote_port, rcv_nxt, rx_count) = {
        let s = &TCPS[idx];
        (s.local_ip, s.remote_ip, s.local_port, s.remote_port, s.rcv_nxt, s.rx_count)
    };
    let total = 20 + data.len();
    if total > TCPBUF.len() {
        return false;
    }
    TCPBUF[0..2].copy_from_slice(&local_port.to_be_bytes());
    TCPBUF[2..4].copy_from_slice(&remote_port.to_be_bytes());
    TCPBUF[4..8].copy_from_slice(&seq.to_be_bytes());
    TCPBUF[8..12].copy_from_slice(&rcv_nxt.to_be_bytes());
    TCPBUF[12] = 0x50;
    TCPBUF[13] = flags;
    // 通告真实可用接收空间（缓冲 4096 − 未取走字节数）：固定窗口会让对端
    // 在缓冲满后继续超发，数据被丢只能等 RTO 重传，接收吞吐骤降。
    let adv_wnd = (TCP_RXBUF - rx_count).min(TCP_WINDOW as usize) as u16;
    TCPBUF[14..16].copy_from_slice(&adv_wnd.to_be_bytes());
    TCPBUF[16..18].copy_from_slice(&0u16.to_be_bytes());
    TCPBUF[18..20].copy_from_slice(&0u16.to_be_bytes());
    TCPBUF[20..total].copy_from_slice(data);
    let ck = net_stack::tcp_checksum(local_ip, remote_ip, &TCPBUF[..total]);
    TCPBUF[16..18].copy_from_slice(&ck.to_be_bytes());
    let sent = net_stack::ip_send(remote_ip, 6, &TCPBUF[..total]) >= 0;
    // 记录发送时间（RTT 采样起点；握手/挥手段也参与估计）
    TCPS[idx].last_send_us = crate::timer::now_us();
    sent
}

unsafe fn send_ack(idx: usize) {
    let seq = TCPS[idx].snd_nxt;
    tcp_xmit(idx, seq, TCP_ACK, &[]);
}

unsafe fn tcp_retransmit(idx: usize) {
    let s = &TCPS[idx];
    if !s.rtx_active {
        return;
    }
    // 超时事件：拥塞窗口收缩（慢启动重启）+ RTO 指数退避（上限 30s）
    const MSS: u32 = 1024;
    let s = &mut TCPS[idx];
    s.ssthresh = (s.cwnd / 2).max(2 * MSS);
    s.cwnd = MSS;
    s.retx_cnt = s.retx_cnt.saturating_add(1);
    s.rto_us = (s.rto_us.saturating_mul(2)).min(30_000_000);
    let (seq, len) = (s.rtx_seq, s.rtx_len);
    if s.state == T_SYN_SENT {
        tcp_xmit(idx, seq, TCP_SYN, &[]);
        return;
    }
    // 服务器侧握手：重发 SYN|ACK（rtx_len == 0，由 T_SYN_RCVD 状态区分）
    if s.state == T_SYN_RCVD {
        tcp_xmit(idx, seq, TCP_SYN | TCP_ACK, &[]);
        return;
    }
    // 挥手阶段的重传：FIN 段（rtx_len == 0）必须重发 FIN|ACK 而非数据段，
    // 否则 FIN 丢失后对端永远等不到关闭通知（M-10 部分修复）。
    if len == 0 && (s.state == T_FIN_WAIT1 || s.state == T_LAST_ACK) {
        tcp_xmit(idx, seq, TCP_FIN | TCP_ACK, &[]);
        return;
    }
    // 拷贝到局部再发送（避免借用冲突）
    let mut tmp = [0u8; TCP_RTXBUF];
    tmp[..len].copy_from_slice(&s.rtx[..len]);
    tcp_xmit(idx, seq, TCP_PSH | TCP_ACK, &tmp[..len]);
}

/// 把数据压入接收环形缓冲，返回实际入队字节数（缓冲满时可能少于 `data.len()`）。
unsafe fn tcp_rx_push(idx: usize, data: &[u8]) -> usize {
    let s = &mut TCPS[idx];
    let mut pushed = 0usize;
    for &b in data {
        if s.rx_count >= TCP_RXBUF {
            break;
        }
        s.rx[(s.rx_head + s.rx_count) % TCP_RXBUF] = b;
        s.rx_count += 1;
        pushed += 1;
    }
    pushed
}

unsafe fn tcp_rx_drain(idx: usize, out: &mut [u8]) -> usize {
    let s = &mut TCPS[idx];
    let n = s.rx_count.min(out.len());
    for i in 0..n {
        out[i] = s.rx[(s.rx_head + i) % TCP_RXBUF];
    }
    s.rx_head = (s.rx_head + n) % TCP_RXBUF;
    s.rx_count -= n;
    n
}

fn find_tcp(src_ip: u32, sport: u16, dport: u16) -> Option<usize> {
    unsafe {
        for i in 0..MAX_TCP {
            let s = &TCPS[i];
            if s.used && s.local_port == dport && s.remote_ip == src_ip && s.remote_port == sport {
                return Some(i);
            }
        }
    }
    None
}

/// TCP 序号环形比较（RFC 1982）：`a` 是否在序列上早于 `b`。
/// 32 位序号回绕后普通 `<`/`>` 失效（累计传输 ~4GB 后必现），
/// 用有符号差判断 ±2^31 窗口内的先后。
fn seq_lt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}
fn seq_gt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}
fn seq_ge(a: u32, b: u32) -> bool {
    a == b || seq_gt(a, b)
}
fn seq_le(a: u32, b: u32) -> bool {
    a == b || seq_lt(a, b)
}

/// 回收长期停留在 T_SYN_RCVD 的子连接（客户端不回 ACK 或已离线），
/// 避免子槽位永久泄漏；同时从监听者的待 accept 队列中移除。
/// 同场回收滞留超时（2MSL）的 T_TIME_WAIT 连接 —— 进入 TIME_WAIT 的前提
/// 是本端应用已 close（主动关闭方），槽位不再被句柄引用，超时回收安全。
unsafe fn tcp_reap_syn_rcvd() {
    let now = crate::timer::now_us();
    for c in 0..MAX_TCP {
        let s = &TCPS[c];
        if !s.used {
            continue;
        }
        if s.state == T_TIME_WAIT {
            if now.saturating_sub(s.last_send_us) >= TIME_WAIT_TIMEOUT_US {
                // 同步清除仍引用该槽位的句柄（R3-M1）：任务若在 tcp_close
                // 的等待循环中被杀，SOCKETS[h] 与 TIME_WAIT 槽一起泄漏；
                // 只清槽位会留下悬空句柄 → 新连接复用槽 c 后被陈旧句柄
                // 误 close（对他人活连接发 FIN）。reaper 持 net_lock，安全。
                for h in 0..MAX_SOCKETS {
                    let so = &mut SOCKETS[h];
                    if so.used && so.ty == SOCK_TCP && so.index == c {
                        *so = EMPTY_SOCKET;
                    }
                }
                TCPS[c] = TcpSock::empty();
            }
            continue;
        }
        if s.state != T_SYN_RCVD {
            continue;
        }
        if now.saturating_sub(s.last_send_us) < SYN_RCVD_TIMEOUT_US {
            continue;
        }
        // 从监听者的待 accept 队列移除
        let lport = s.local_port;
        for l in 0..MAX_TCP {
            let lp = &mut TCPS[l];
            if !lp.used || lp.state != T_LISTEN || lp.local_port != lport {
                continue;
            }
            for i in 0..lp.pending_count {
                if lp.pending[i] == c {
                    for k in i + 1..lp.pending_count {
                        lp.pending[k - 1] = lp.pending[k];
                    }
                    lp.pending_count -= 1;
                    lp.pending[lp.pending_count] = usize::MAX;
                    break;
                }
            }
        }
        // 回收子槽位（保持 MAX_TCP 计数一致：used=false）
        TCPS[c] = TcpSock::empty();
    }
}

/// 网卡收到的 TCP 段入口（由 net_stack 分发调用）。
pub fn tcp_input(src_ip: u32, seg: &[u8]) {
    // 每个 TCP 段到来时顺带回收过期的 T_SYN_RCVD 子连接
    unsafe { tcp_reap_syn_rcvd() };
    if seg.len() < 20 {
        return;
    }
    let sport = u16::from_be_bytes([seg[0], seg[1]]);
    let dport = u16::from_be_bytes([seg[2], seg[3]]);
    let seq = u32::from_be_bytes([seg[4], seg[5], seg[6], seg[7]]);
    let ack = u32::from_be_bytes([seg[8], seg[9], seg[10], seg[11]]);
    let flags = seg[13];
    let dataoff = ((seg[12] >> 4) as usize) * 4;
    if dataoff < 20 || seg.len() < dataoff {
        return;
    }
    let data = &seg[dataoff..];

    let Some(idx) = find_tcp(src_ip, sport, dport) else {
        // 未匹配到既有连接：若是 SYN 且目标端口在监听 → 建立服务器侧连接
        if flags & TCP_SYN != 0 && flags & TCP_ACK == 0 {            unsafe {
                for l in 0..MAX_TCP {
                    let lsn = &TCPS[l];
                    if lsn.used && lsn.state == T_LISTEN && lsn.local_port == dport {
                        // 找一个空闲槽位做子连接（不能是监听者自身）
                        let mut child = usize::MAX;
                        for i in 0..MAX_TCP {
                            if !TCPS[i].used {
                                child = i;
                                break;
                            }
                        }
                        if child == usize::MAX {
                            return; // 连接槽位耗尽：静默拒绝
                        }
                        let iss = next_iss();
                        TCPS[child] = TcpSock {
                            used: true,
                            state: T_SYN_RCVD,
                            local_ip: lsn.local_ip,
                            local_port: dport,
                            remote_ip: src_ip,
                            remote_port: sport,
                            iss,
                            snd_nxt: iss.wrapping_add(1),
                            snd_una: iss.wrapping_add(1),
                            rcv_nxt: seq.wrapping_add(1),
                            rx: [0; TCP_RXBUF],
                            rx_head: 0,
                            rx_count: 0,
                            rtx: [0; TCP_RTXBUF],
                            rtx_len: 0,
                            rtx_seq: 0,
                            rtx_active: false,
                            fin_sent: false,
                            fin_rtx_pending: false,
                            peer_fin: false,
                            pending: [usize::MAX; MAX_TCP],
                            pending_count: 0,
                            cwnd: 4 * 1024,
                            ssthresh: 64 * 1024,
                            rto_us: RETX_US,
                            last_send_us: 0,
                            retx_cnt: 0,
                        };
                        // 发送 SYN-ACK
                        tcp_xmit(child, iss, TCP_SYN | TCP_ACK, &[]);
                        // 登记 SYN-ACK 重传：rtx_len == 0 表示握手段，
                        // 供 tcp_retransmit 按 T_SYN_RCVD 状态重发 SYN|ACK
                        TCPS[child].rtx_active = true;
                        TCPS[child].rtx_seq = iss;
                        TCPS[child].rtx_len = 0;
                        // 挂到监听者的待 accept 队列
                        let lp = &mut TCPS[l];
                        if lp.pending_count < MAX_TCP {
                            lp.pending[lp.pending_count] = child;
                            lp.pending_count += 1;
                        }
                        return;
                    }
                }
            }
        }
        return;
    };

    // RST：连接被对端重置（拒绝/崩溃），标记关闭并清除重传状态。
    // ★ 校验（RFC 793 思路）：seq == rcv_nxt 的裸 RST，或 ack == snd_nxt
    // 的带 ACK RST 才接受 —— 此前无任何校验，off-path 伪造 RST（校验和
    // 可按已知四元组重算）即可杀死任意连接。
    if flags & TCP_RST != 0 {
        let ok = seq == unsafe { TCPS[idx].rcv_nxt }
            || (flags & TCP_ACK != 0 && ack == unsafe { TCPS[idx].snd_nxt });
        if ok {
            unsafe {
                TCPS[idx].state = T_CLOSED;
                TCPS[idx].rtx_active = false;
            }
        }
        return;
    }

    // 死亡套接字（CLOSED/TIME_WAIT）收到对端同四元组的裸 SYN：此前按
    // 4 元组匹配静默吞掉（无 SYN-ACK 也无 RST），客户端重连只能空等到
    // 自身超时。回 RST（ack=seq+1）让对端立即换端口重试。
    if flags & TCP_SYN != 0 && flags & TCP_ACK == 0 {
        let st = unsafe { TCPS[idx].state };
        if st == T_CLOSED || st == T_TIME_WAIT {
            unsafe {
                TCPS[idx].rcv_nxt = seq.wrapping_add(1); // tcp_xmit 的 ack 字段取 rcv_nxt
                tcp_xmit(idx, 0, TCP_RST | TCP_ACK, &[]);
            }
            return;
        }
    }

    let state = unsafe { TCPS[idx].state };
    match state {
        T_SYN_SENT => {
            if flags & TCP_SYN != 0 && flags & TCP_ACK != 0 {
                // 校验 ACK == ISS+1（对端确认了我们发起的序号），防止异常/伪造 SYN-ACK 破坏状态
                if unsafe { ack != TCPS[idx].iss.wrapping_add(1) } {
                    return;
                }
                unsafe {
                    TCPS[idx].rcv_nxt = seq.wrapping_add(1);
                    TCPS[idx].snd_una = ack;
                    TCPS[idx].snd_nxt = ack;
                    TCPS[idx].state = T_ESTABLISHED;
                    TCPS[idx].rtx_active = false;
                }
                unsafe { send_ack(idx) };
            }
        }
        T_SYN_RCVD => {
            // 客户端重传 SYN（丢失了 SYN-ACK）：用同一 iss 重发 SYN|ACK。
            // 此前这种重 SYN 会被静默丢弃，客户端永远进不了 ESTAB。
            if flags & TCP_SYN != 0 && flags & TCP_ACK == 0 {
                unsafe {
                    let iss = TCPS[idx].iss;
                    TCPS[idx].rtx_active = true;
                    TCPS[idx].rtx_seq = iss;
                    TCPS[idx].rtx_len = 0;
                    tcp_xmit(idx, iss, TCP_SYN | TCP_ACK, &[]);
                }
                return;
            }
            // 客户端对 SYN-ACK 的 ACK：握手完成
            // ★ 校验 ack == iss+1（与客户端侧对称）：盲目接受任意 ACK 会让
            // 伪造/迟到的旧 ACK 抢先完成握手并把 snd_una 回退，连接失步。
            if flags & TCP_ACK != 0 && ack == unsafe { TCPS[idx].iss }.wrapping_add(1) {
                unsafe {
                    TCPS[idx].snd_una = ack;
                    TCPS[idx].state = T_ESTABLISHED;
                    TCPS[idx].rtx_active = false;
                }
            }
        }
        T_ESTABLISHED | T_FIN_WAIT1 | T_FIN_WAIT2 | T_CLOSE_WAIT | T_LAST_ACK => {
            let mut need_ack = false;
            if flags & TCP_ACK != 0 {
                unsafe {
                    let new_ack = seq_gt(ack, TCPS[idx].snd_una);
                    // 序号回绕安全比较：ack 比 snd_una 新才推进。
                    // ★ 上界校验：ack 不得越过 snd_nxt —— 此前无上界，伪造/
                    // 迟到的超大 ACK 会把 snd_una 推过 snd_nxt，未上线路的
                    // 数据被误判"已确认"（rtx 误清除、连接失步）。越界 ACK
                    // 只是不推进 snd_una，段内数据仍按下方流程处理。
                    if new_ack && seq_le(ack, TCPS[idx].snd_nxt) {
                        TCPS[idx].snd_una = ack;
                    }
                    if TCPS[idx].rtx_active
                        && seq_ge(ack, TCPS[idx].rtx_seq.wrapping_add(TCPS[idx].rtx_len as u32))
                    {
                        TCPS[idx].rtx_active = false;
                        // 重传成功：退避复位，RTO 恢复基础值
                        TCPS[idx].retx_cnt = 0;
                        TCPS[idx].rto_us = RETX_US;
                        // close 时数据段占用了重传槽位：数据确认后补登 FIN
                        // 重传（FIN 序号 = snd_nxt-1，rtx_len=0 由挥手分支重发）
                        if TCPS[idx].fin_rtx_pending
                            && (TCPS[idx].state == T_FIN_WAIT1 || TCPS[idx].state == T_LAST_ACK)
                        {
                            TCPS[idx].fin_rtx_pending = false;
                            TCPS[idx].rtx_active = true;
                            TCPS[idx].rtx_seq = TCPS[idx].snd_nxt.wrapping_sub(1);
                            TCPS[idx].rtx_len = 0;
                        }
                    }
                    // 新 ACK：RTT 采样 + 拥塞窗口推进（慢启动 / 拥塞避免）
                    if new_ack {
                        const MSS: u32 = 1024;
                        let now = crate::timer::now_us();
                        let last = TCPS[idx].last_send_us;
                        if last != 0 && now > last {
                            let rtt = now - last;
                            // 指数加权平滑 RTT（α=0.25），RTO = max(2×RTT, 200ms)
                            let est = (TCPS[idx].rto_us.saturating_mul(3) + rtt.saturating_mul(2)) / 5;
                            TCPS[idx].rto_us = (est.max(200_000)).min(4_000_000);
                        }
                        if TCPS[idx].cwnd < TCPS[idx].ssthresh {
                            TCPS[idx].cwnd = TCPS[idx].cwnd.saturating_add(MSS); // 慢启动
                        } else {
                            // 拥塞避免：每 RTT 增 1 MSS
                            TCPS[idx].cwnd = TCPS[idx]
                                .cwnd
                                .saturating_add((MSS * MSS) / TCPS[idx].cwnd.max(MSS));
                        }
                    }
                    // 我方 FIN 已被确认
                    if TCPS[idx].fin_sent && seq_ge(TCPS[idx].snd_una, TCPS[idx].snd_nxt) {
                        TCPS[idx].state = if TCPS[idx].state == T_FIN_WAIT1 {
                            T_FIN_WAIT2
                        } else if TCPS[idx].state == T_LAST_ACK {
                            // 被动关闭：对端已 ACK 我方 FIN，关闭完成（此前 T_LAST_ACK
                            // 不在 ACK 处理分支，close() 每次都干等 CLOSE_TIMEOUT_US）
                            T_CLOSED
                        } else {
                            TCPS[idx].state
                        };
                    }
                }
            }
            if !data.is_empty() {
                unsafe {
                    if seq == TCPS[idx].rcv_nxt {
                        let pushed = tcp_rx_push(idx, data);
                        // 只按实际入队字节数推进 rcv_nxt：若缓冲已满导致部分数据未入队，
                        // 不推进、不发 ACK，让对端超时重传，避免「丢弃但 ACK」造成永久丢数据。
                        TCPS[idx].rcv_nxt = seq.wrapping_add(pushed as u32);
                        need_ack = pushed > 0;
                    } else if seq_lt(seq, TCPS[idx].rcv_nxt) {
                        // 旧/重叠段：对端可能重传**整段**（上次入队时缓冲满只进了一部分）。
                        // 段尾若超过 rcv_nxt 则含未接收的新数据，补入队该尾部（H-5 补全：
                        // 此前整段重传被当作纯重复丢弃 → 尾部数据永久丢失）；
                        // 段尾不超 rcv_nxt 才是纯重复，回 ACK 帮助对端推进。
                        let seg_end = seq.wrapping_add(data.len() as u32);
                        if seq_gt(seg_end, TCPS[idx].rcv_nxt) {
                            let skip = (TCPS[idx].rcv_nxt - seq) as usize;
                            let tail = &data[skip.min(data.len())..];
                            let pushed = tcp_rx_push(idx, tail);
                            if pushed > 0 {
                                TCPS[idx].rcv_nxt =
                                    TCPS[idx].rcv_nxt.wrapping_add(pushed as u32);
                            }
                            need_ack = pushed > 0;
                        } else {
                            need_ack = true;
                        }
                    }
                    // seq > rcv_nxt：乱序/丢段，静默等待对端按序重传
                }
            }
            if flags & TCP_FIN != 0 {
                unsafe {
                    // 仅当 FIN 之前的数据全部已入队（FIN 序号 == rcv_nxt）才处理：
                    // 若缓冲满导致 data 未完全入队，FIN 序号 > rcv_nxt，此时处理会
                    // ACK 掉未接收的数据（H-5 修复补全：满缓冲 + data+FIN 场景）。
                    let fin_seq = seq.wrapping_add(data.len() as u32);
                    if fin_seq == TCPS[idx].rcv_nxt {
                        TCPS[idx].rcv_nxt = TCPS[idx].rcv_nxt.wrapping_add(1);
                        TCPS[idx].peer_fin = true;
                        if TCPS[idx].state == T_ESTABLISHED {
                            TCPS[idx].state = T_CLOSE_WAIT;
                        } else if TCPS[idx].state == T_FIN_WAIT2 {
                            TCPS[idx].state = T_TIME_WAIT;
                            // 2MSL 计时锚点：tcp_reap_syn_rcvd 超时自动回收
                            TCPS[idx].last_send_us = crate::timer::now_us();
                        }
                        need_ack = true;
                    } else if seq_lt(fin_seq, TCPS[idx].rcv_nxt) {
                        // 对端 FIN 重传（我方首个 ACK 已丢）：重新 ACK 让对端
                        // 结束重传循环 —— 与 TIME_WAIT 分支同语义。此前不回
                        // ACK，对端只能 RTO 重传到放弃，拆除卡在 CLOSE_WAIT
                        // 直到本端应用 close()。
                        need_ack = true;
                    }
                }
            }
            if need_ack {
                unsafe { send_ack(idx) };
            }
        }
        T_TIME_WAIT => {
            // 我方 ACK 丢失、对端 FIN 重传：重新 ACK（rcv_nxt 已越过 FIN），
            // 让对端结束重传循环 —— 此前落在 `_ => {}`，对端只能 RTO 重传
            // 到放弃，本端 TIME_WAIT 也只能靠用户 close 解除。
            if flags & TCP_FIN != 0 {
                unsafe { send_ack(idx) };
            }
        }
        _ => {}
    }
}

fn tcp_connect(idx: usize, ip: u32, port: u16) -> i32 {
    // 目标端口 0 非法（同 UDP connect）
    if port == 0 {
        return -22; // -EINVAL
    }
    // 先解析 ARP，避免握手途中阻塞（resolve 内部持锁）
    if !net_stack::resolve(ip) {
        return -1;
    }
    let iss = next_iss();
    // 连接初始化 + 首个 SYN 段：原子完成（SYN 未发出前连接状态不可见）
    net_stack::net_lock();
    unsafe {
        let s = &mut TCPS[idx];
        s.remote_ip = ip;
        s.remote_port = port;
        if s.local_port == 0 {
            s.local_port = net_stack::next_port();
        }
        s.iss = iss;
        s.snd_nxt = iss;
        s.snd_una = iss;
        s.rcv_nxt = 0;
        s.rx_head = 0;
        s.rx_count = 0;
        s.fin_sent = false;
        s.fin_rtx_pending = false;
        s.peer_fin = false;
        s.rtx_active = true;
        s.rtx_seq = iss;
        s.rtx_len = 0;
        s.state = T_SYN_SENT;
    }
    unsafe { tcp_xmit(idx, iss, TCP_SYN, &[]) };
    unsafe {
        TCPS[idx].snd_nxt = iss.wrapping_add(1);
    }
    net_stack::net_unlock();

    // 等待握手完成：状态检查与重传段持锁，poll/yield 不持锁
    let start = crate::timer::now_us();
    let mut next_retx = start + RETX_US;
    while crate::timer::now_us() - start < CONNECT_TIMEOUT_US {
        net_stack::net_lock();
        let action = unsafe {
            if TCPS[idx].state == T_ESTABLISHED {
                Some(0)
            } else if TCPS[idx].state == T_CLOSED {
                // 连接被拒绝（对端 RST，如端口未监听）：立即失败，避免干等超时。
                Some(-1)
            } else {
                None
            }
        };
        if let Some(rc) = action {
            net_stack::net_unlock();
            return rc;
        }
        let now = crate::timer::now_us();
        if now >= next_retx {
            unsafe { tcp_retransmit(idx) };
            next_retx = now + RETX_US;
        }
        net_stack::net_unlock();
        net_stack::poll();
        sched_cfs::yield_now();
    }
    -1
}

fn tcp_send(idx: usize, data: &[u8]) -> i32 {
    if data.is_empty() {
        return 0;
    }
    if data.len() > TCP_RTXBUF {
        return -1;
    }
    // 段必须能完整穿越下层缓冲，否则会被静默丢帧但 snd_nxt 照常推进 →
    // 连接永久失步。且必须满足标准以太网 MTU：1500 − 20(IP) − 20(TCP) =
    // 1460 字节载荷——按内部缓冲极限（~1994）放行的段组出 2048 字节帧，
    // QEMU slirp 侥幸可过，真实网卡/交换机一律丢弃 → 大段黑洞、连接卡死。
    const TCP_MSS: usize = 1460;
    if data.len() > TCP_MSS {
        return -1;
    }
    if unsafe { 54 + data.len() > TCPBUF.len() } {
        return -1;
    }
    // 状态检查 + 重传登记 + 发段：原子完成（段未上线路前不得推进 snd_nxt）
    net_stack::net_lock();
    if unsafe { TCPS[idx].state != T_ESTABLISHED } {
        net_stack::net_unlock();
        return -1;
    }
    // 记录待重传段
    let seq = unsafe { TCPS[idx].snd_nxt };
    let sent = unsafe {
        let s = &mut TCPS[idx];
        s.rtx[..data.len()].copy_from_slice(data);
        s.rtx_len = data.len();
        s.rtx_seq = seq;
        s.rtx_active = true;
        tcp_xmit(idx, seq, TCP_PSH | TCP_ACK, data)
    };
    if !sent {
        // 段未上线路：撤销重传登记、不推进 snd_nxt，报错给调用方
        unsafe {
            TCPS[idx].rtx_active = false;
        }
        net_stack::net_unlock();
        return -1;
    }
    unsafe {
        TCPS[idx].snd_nxt = seq.wrapping_add(data.len() as u32);
    }
    net_stack::net_unlock();

    // 等待 ACK：检查与重传段持锁，poll/yield 不持锁
    let start = crate::timer::now_us();
    let mut next_retx = start + RETX_US;
    while crate::timer::now_us() - start < SEND_TIMEOUT_US {
        net_stack::net_lock();
        // 连接已被 RST：立即失败（此前只查 acked 与重传，RST 后空转满超时）
        if unsafe { TCPS[idx].state == T_CLOSED } {
            net_stack::net_unlock();
            return -1;
        }
        // 序号回绕安全比较：snd_una 是否已越过段尾（原始 >= 在 32 位
        // 回绕后可能误判，导致 send 提前返回 / 永远等不到）
        let acked = unsafe { seq_ge(TCPS[idx].snd_una, seq.wrapping_add(data.len() as u32)) };
        if acked {
            net_stack::net_unlock();
            return data.len() as i32;
        }
        let now = crate::timer::now_us();
        if now >= next_retx {
            unsafe { tcp_retransmit(idx) };
            next_retx = now + RETX_US;
        }
        net_stack::net_unlock();
        net_stack::poll();
        sched_cfs::yield_now();
    }
    -1
}

fn tcp_recv(idx: usize, buf: &mut [u8]) -> i32 {
    let start = crate::timer::now_us();
    loop {
        // 出队/EOF 判定段持锁；poll/yield 不持锁
        net_stack::net_lock();
        let n = unsafe { tcp_rx_drain(idx, buf) };
        if n > 0 {
            // 窗口更新：取走数据后立即 ACK，通告重新打开的接收窗口——
            // 否则对端在填满旧窗口后只能靠零窗口探测/RTO 逐段恢复。
            unsafe { send_ack(idx) };
            net_stack::net_unlock();
            return n as i32;
        }
        // 对端已关闭（FIN）或连接被重置（RST → T_CLOSED）且无残留数据 → EOF。
        // 此前 RST 只置 T_CLOSED 不置 peer_fin，recv 会干等 RECV_TIMEOUT_US（60s）。
        let eof = unsafe { (TCPS[idx].peer_fin || TCPS[idx].state == T_CLOSED) && TCPS[idx].rx_count == 0 };
        net_stack::net_unlock();
        if eof {
            return 0;
        }
        net_stack::poll();
        net_stack::net_lock();
        let n2 = unsafe { tcp_rx_drain(idx, buf) };
        if n2 > 0 {
            net_stack::net_unlock();
            return n2 as i32;
        }
        let eof2 = unsafe { (TCPS[idx].peer_fin || TCPS[idx].state == T_CLOSED) && TCPS[idx].rx_count == 0 };
        // 接收循环不做重传驱动：若同 socket 另一线程（fork 继承 fd）有在途
        // send/FIN 重传，此处独立重发会制造重复段并把退避副作用（rto 翻倍、
        // cwnd 压制）错误施加 —— 重传只由 send/close/超时路径负责。
        net_stack::net_unlock();
        if eof2 {
            return 0;
        }
        let now = crate::timer::now_us();
        if now - start > RECV_TIMEOUT_US {
            return -1;
        }
        sched_cfs::yield_now();
    }
}

fn tcp_close(idx: usize) {
    // FIN 发送与重传登记：原子完成
    net_stack::net_lock();
    unsafe {
        let s = &mut TCPS[idx];
        match s.state {
            T_ESTABLISHED => {
                s.fin_sent = true;
                s.state = T_FIN_WAIT1;
                let seq = s.snd_nxt;
                s.snd_nxt = s.snd_nxt.wrapping_add(1);
                tcp_xmit(idx, seq, TCP_FIN | TCP_ACK, &[]);
                // ★ 数据段重传仍登记（send 超时未获 ACK）时不得覆盖重传槽位
                // ——否则 tcp_send 已丢失/未确认的数据段永远不再上线，对端
                // 停在等数据（对空洞回重复 ACK，不确认 FIN），双向死等超时
                // 后连接静默拆除、数据丢失。数据确认后由 ACK 处理补登 FIN。
                if s.rtx_active && s.rtx_len > 0 && !seq_ge(s.snd_una, s.rtx_seq.wrapping_add(s.rtx_len as u32)) {
                    s.fin_rtx_pending = true;
                } else {
                    // 登记 FIN 重传：FIN 丢失时由 tcp_retransmit 重发（而非旧数据段）
                    s.rtx_active = true;
                    s.rtx_seq = seq;
                    s.rtx_len = 0;
                }
            }
            T_CLOSE_WAIT => {
                s.fin_sent = true;
                s.state = T_LAST_ACK;
                let seq = s.snd_nxt;
                s.snd_nxt = s.snd_nxt.wrapping_add(1);
                tcp_xmit(idx, seq, TCP_FIN | TCP_ACK, &[]);
                if s.rtx_active && s.rtx_len > 0 && !seq_ge(s.snd_una, s.rtx_seq.wrapping_add(s.rtx_len as u32)) {
                    s.fin_rtx_pending = true;
                } else {
                    s.rtx_active = true;
                    s.rtx_seq = seq;
                    s.rtx_len = 0;
                }
            }
            _ => {}
        }
    }
    net_stack::net_unlock();

    // 等待对端 ACK + FIN（或超时）：检查与重传段持锁，poll/yield 不持锁
    let start = crate::timer::now_us();
    let mut next_retx = start + RETX_US;
    while crate::timer::now_us() - start < CLOSE_TIMEOUT_US {
        net_stack::net_lock();
        let done = unsafe {
            let s = &TCPS[idx];
            // 序号回绕安全比较：累计传输 ≥4GB 后普通 >= 失效（全文件唯一
            // 残留的普通比较），close 要么干等超时、要么误判 FIN 已确认。
            (s.fin_sent && seq_ge(s.snd_una, s.snd_nxt) && s.peer_fin) || !s.fin_sent
        };
        if done {
            net_stack::net_unlock();
            break;
        }
        let now = crate::timer::now_us();
        if now >= next_retx {
            unsafe { tcp_retransmit(idx) };
            next_retx = now + RETX_US;
        }
        net_stack::net_unlock();
        net_stack::poll();
        sched_cfs::yield_now();
    }
    // 释放套接字
    net_stack::net_lock();
    for h in 0..MAX_SOCKETS {
        unsafe {
            if SOCKETS[h].used && SOCKETS[h].ty == SOCK_TCP && SOCKETS[h].index == idx {
                SOCKETS[h] = EMPTY_SOCKET;
            }
        }
    }
    unsafe {
        TCPS[idx] = TcpSock::empty();
    }
    net_stack::net_unlock();
}
