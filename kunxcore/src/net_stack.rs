//! 网络协议栈：Ethernet / ARP / IPv4 / ICMP / UDP / TCP 解复用 + DNS 客户端。
//!
//! - 数据链路层：以太网帧组装（`eth_send`）与分发（`dispatch`）。
//! - ARP：缓存 + 请求/应答，`arp_resolve` 阻塞解析目标 MAC。
//! - IPv4：头组装与校验和，`ip_send` 统一出口。
//! - ICMP：echo 请求/应答（ping 用）。
//! - UDP：`udp_send` 组装并发送；收到的 UDP 包按端口分发给 socket 层。
//! - TCP：收到的 TCP 段转交 `socket::tcp_input`，由 socket 层维护连接状态。
//! - DNS：`dns_resolve` 用 UDP 查询 A 记录。
//!
//! 所有协议状态只在进程上下文（socket 操作的轮询循环）中修改；中断处理仅
//! 把硬件环里的包搬进软件接收队列（见 `net_virtio`），中断与任务之间无需锁。
//! 但任务**之间**仍有并发：syscall 上下文 IRQ 使能、可被 tick 抢占切任务，
//! 两个任务的联网操作交错会互踩下方的全局组装缓冲与 socket 队列，因此所有
//! 入口经 [`net_lock`] 串行化（见下）。

use crate::net_virtio;
use crate::sched_cfs;
use core::sync::atomic::{AtomicU32, Ordering};

// ==================== 全局网络锁 ====================

/// 保护对象：FRAME/IPBUF/UDPBUF/CHECKBUF/RXFRAME/DNSBUF 组装缓冲、
/// ARP/DNS/DHCP/ICMP 全局状态、socket 表与收发队列（socket.rs 亦经此锁）。
///
/// 实现与 `fs_simple` 的 FS 锁同源：按持有任务 tid 可重入（嵌套路径
/// poll→dispatch→input→send、udp_send→ip_send→arp_resolve→poll 等靠重入
/// 放行），等待者 1ms 睡眠退避（进入 Blocked 让出 CPU，不忙等、无优先级
/// 饿死）。不屏蔽 IRQ：RX 分发只发生在任务上下文 `poll()`，virtio 驱动环
/// 操作（drain/send/recv_into）内部已自行屏蔽 IRQ。
///
/// 持锁规则：**长等待循环（recv/accept/connect 等 poll+yield 循环）不得持锁
/// 跨过 poll/yield**——只包住状态读写段，否则一个阻塞中的任务会让其他任务
/// 的联网操作等待到其超时。短操作（组包+发送、表读写、dns/dhcp/ping 等有界
/// 等待）整体持锁。任务被杀时由 [`net_cleanup_tid`] 释放锁（防泄漏）。
const NET_LOCK_FREE: u32 = 0;
static NET_LOCK: AtomicU32 = AtomicU32::new(NET_LOCK_FREE);
static NET_LOCK_DEPTH: AtomicU32 = AtomicU32::new(0);

/// 取全局网络锁（按 tid 可重入；等待者睡眠退避）。
pub fn net_lock() {
    let my = crate::sched_cfs::current_tid().wrapping_add(1);
    if NET_LOCK.load(Ordering::Acquire) == my {
        NET_LOCK_DEPTH.fetch_add(1, Ordering::Relaxed);
        return;
    }
    loop {
        if NET_LOCK
            .compare_exchange(NET_LOCK_FREE, my, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            NET_LOCK_DEPTH.store(1, Ordering::Relaxed);
            return;
        }
        crate::sched_cfs::sleep_until(crate::timer::now_us() + 1000);
    }
}

/// 释放全局网络锁。
pub fn net_unlock() {
    let my = crate::sched_cfs::current_tid().wrapping_add(1);
    if NET_LOCK.load(Ordering::Acquire) != my {
        // 防御：非持有者调用（全局状态曾被污染的兜底）。直接释放，避免锁被
        // 永久持有导致整个网络栈死锁。
        NET_LOCK.store(NET_LOCK_FREE, Ordering::Release);
        NET_LOCK_DEPTH.store(0, Ordering::Relaxed);
        return;
    }
    let d = NET_LOCK_DEPTH.load(Ordering::Relaxed);
    if d <= 1 {
        NET_LOCK.store(NET_LOCK_FREE, Ordering::Release);
        NET_LOCK_DEPTH.store(0, Ordering::Relaxed);
    } else {
        NET_LOCK_DEPTH.store(d - 1, Ordering::Relaxed);
    }
}

/// 任务 `tid` 退出/被杀时清掉其持有的网络锁：否则持锁任务死亡后锁永久被占，
/// 所有后续联网操作在退避睡眠中无限等待。
///
/// 由 `sched_cfs::kill_pid`（进程退出/终止统一清理路径）调用。
pub fn net_cleanup_tid(tid: u32) {
    let my = tid.wrapping_add(1);
    if NET_LOCK.load(Ordering::Acquire) == my {
        NET_LOCK.store(NET_LOCK_FREE, Ordering::Release);
        NET_LOCK_DEPTH.store(0, Ordering::Relaxed);
    }
}

// ==================== 常量 ====================

const ETH_IP: u16 = 0x0800;
const ETH_ARP: u16 = 0x0806;

const IPPROTO_ICMP: u8 = 1;
const IPPROTO_TCP: u8 = 6;
const IPPROTO_UDP: u8 = 17;

const ARP_HTYPE_ETH: u16 = 1;
const ARP_OP_REQUEST: u16 = 1;
const ARP_OP_REPLY: u16 = 2;

const DNS_PORT: u16 = 53;

// ---- DHCP（RFC 2131 极简客户端） ----
const DHCP_PORT_SRV: u16 = 67;
const DHCP_PORT_CLI: u16 = 68;
const DHCP_TIMEOUT_US: u64 = 15_000_000;
const DHCP_RETX_US: u64 = 2_000_000;

const ARP_MAX: usize = 8;
/// ARP 条目生存期（简化 ARP 缓存 TTL，参考 Linux base_reachable_time）。
/// lookup 命中超龄条目即作废并重新解析。
const ARP_ENTRY_TTL_US: u64 = 60_000_000;
const ARP_TIMEOUT_US: u64 = 2_000_000;
const ARP_RETX_US: u64 = 300_000;

const PING_TIMEOUT_US: u64 = 3_000_000;
const DNS_TIMEOUT_US: u64 = 20_000_000;
const DNS_RETX_US: u64 = 2_000_000;

// ==================== 配置 ====================

static mut LOCAL_MAC: [u8; 6] = [0; 6];
static mut LOCAL_IP: u32 = 0;
static mut GATEWAY_IP: u32 = 0;
static mut NETMASK: u32 = 0;
static mut DNS_IP: u32 = 0;

/// 是否正在分发（arp_resolve 在分发路径内不阻塞，避免递归）。
static mut IN_DISPATCH: bool = false;

/// IP 标识 / 临时端口计数器。
static mut IDENT: u16 = 0;
static mut EPHEMERAL: u16 = 0x4000;

// ==================== 组装缓冲（进程上下文专用，单线程） ====================

static mut FRAME: [u8; 2048] = [0; 2048]; // 以太网帧
static mut IPBUF: [u8; 2048] = [0; 2048]; // IP 头 + 载荷
static mut UDPBUF: [u8; 2048] = [0; 2048]; // UDP 段
static mut RXFRAME: [u8; 2048] = [0; 2048]; // 收包分发缓冲
// 12 字节伪头 + 最长 TCP 段（20B 头 + 最大载荷 2028B）= 2060B，取 2100 留余量。
// 此前 CHECKBUF(2048) 放不下 12+seg.len()=2060，transport_checksum 直接返回 0，
// 发出的 TCP 段校验和恒为 0 → 对端一律丢弃。
static mut CHECKBUF: [u8; 2100] = [0; 2100]; // 传输层伪头校验和
static mut DNSBUF: [u8; 512] = [0; 512]; // DNS 查询

// ==================== ARP 缓存 ====================

#[derive(Clone, Copy)]
struct ArpEntry {
    used: bool,
    ip: u32,
    mac: [u8; 6],
    /// 最近一次使用/学习时刻（微秒），用于缓存淘汰（满了逐最旧）。
    age: u64,
}

const EMPTY_ARP: ArpEntry = ArpEntry { used: false, ip: 0, mac: [0; 6], age: 0 };
static mut ARP_CACHE: [ArpEntry; ARP_MAX] = [EMPTY_ARP; ARP_MAX];

// ==================== ICMP / DNS 状态 ====================

static mut ICMP_ID: u16 = 0x1234;
static mut PING_SEQ: u16 = 0;
static mut PING_GOT_REPLY: bool = false;

static mut DNS_SRC_PORT: u16 = 0;
static mut DNS_ID: u16 = 0;
static mut DNS_RESULT_IP: u32 = 0;
static mut DNS_DONE: bool = false;

// ==================== 工具函数 ====================

/// 递增 IP 标识。
fn next_ident() -> u16 {
    net_lock();
    let r = unsafe {
        IDENT = IDENT.wrapping_add(1);
        IDENT
    };
    net_unlock();
    r
}

/// 分配临时源端口（socket 层用；跳过已被占用的端口，返回 0 表示失败）。
pub fn next_port() -> u16 {
    net_lock();
    let r = unsafe {
        // 最多尝试 128 次：跳过 UDP/TCP 已绑定的端口，避免与显式 bind 或
        // 其它连接自动分配的端口冲突（此前只自增，可能撞上已绑定端口）。
        let mut found = 0u16;
        for _ in 0..128 {
            EPHEMERAL = EPHEMERAL.wrapping_add(1);
            if EPHEMERAL < 0x4000 {
                EPHEMERAL = 0x4000;
            }
            if !crate::socket::port_in_use(EPHEMERAL) {
                found = EPHEMERAL;
                break;
            }
        }
        found
    };
    net_unlock();
    r
}

/// Internet 校验和（RFC 1071）。
fn checksum(data: &[u8]) -> u16 {
    let mut sum = 0u32;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += ((data[i] as u32) << 8) | data[i + 1] as u32;
        i += 2;
    }
    if i < data.len() {
        sum += (data[i] as u32) << 8;
    }
    while (sum >> 16) != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// TCP 段校验和（含伪头），供 socket 层组装 TCP 段时使用。
pub fn tcp_checksum(src: u32, dst: u32, seg: &[u8]) -> u16 {
    transport_checksum(src, dst, IPPROTO_TCP, seg)
}

/// 预解析目标 MAC（阻塞直到 ARP 完成）。返回是否解析成功。
///
/// 供发起连接的 socket 层在发送前预热 ARP 缓存，避免后续 `ip_send` 再阻塞。
pub fn resolve(ip: u32) -> bool {
    net_lock();
    let r = arp_resolve(next_hop(ip)).is_some();
    net_unlock();
    r
}

/// 计算下一跳 IP：目的地址不在本地子网时经网关转发。
fn next_hop(dst_ip: u32) -> u32 {
    unsafe {
        if (dst_ip & NETMASK) == (LOCAL_IP & NETMASK) {
            dst_ip
        } else {
            GATEWAY_IP
        }
    }
}

/// 传输层校验和（含 12 字节伪头：src/dst/zero/proto/len）。
fn transport_checksum(src: u32, dst: u32, proto: u8, seg: &[u8]) -> u16 {
    unsafe {
        let n = 12 + seg.len();
        if n > CHECKBUF.len() {
            return 0;
        }
        CHECKBUF[0..4].copy_from_slice(&src.to_be_bytes());
        CHECKBUF[4..8].copy_from_slice(&dst.to_be_bytes());
        CHECKBUF[8] = 0;
        CHECKBUF[9] = proto;
        CHECKBUF[10..12].copy_from_slice(&(seg.len() as u16).to_be_bytes());
        CHECKBUF[12..n].copy_from_slice(seg);
        checksum(&CHECKBUF[..n])
    }
}

/// 回环注入前按新源 IP 重算传输层校验和（原地改写段内校验和字段）。
/// 计算前先把校验和字段清零（校验和覆盖含自身字段的整段，残留旧值会使
/// 结果错位）。TCP 校验和在头偏移 16；UDP 在偏移 6 且 0 表示"未计算"。
fn fix_transport_checksum(src: u32, dst: u32, proto: u8, seg: &mut [u8]) {
    match proto {
        6 => {
            if seg.len() >= 18 {
                seg[16..18].copy_from_slice(&[0, 0]);
                let ck = transport_checksum(src, dst, 6, seg);
                seg[16..18].copy_from_slice(&ck.to_be_bytes());
            }
        }
        17 => {
            if seg.len() >= 8 && seg[6..8] != [0, 0] {
                seg[6..8].copy_from_slice(&[0, 0]);
                let ck = transport_checksum(src, dst, 17, seg);
                seg[6..8].copy_from_slice(&ck.to_be_bytes());
            }
        }
        _ => {}
    }
}

// ==================== 初始化 ====================

/// 初始化协议栈配置。
pub fn init(mac: [u8; 6], ip: u32, gateway: u32, netmask: u32, dns: u32) {
    net_lock();
    unsafe {
        LOCAL_MAC = mac;
        LOCAL_IP = ip;
        GATEWAY_IP = gateway;
        NETMASK = netmask;
        DNS_IP = dns;
        ICMP_ID = 0x1234;
        PING_SEQ = 0;
        PING_GOT_REPLY = false;
        DNS_SRC_PORT = 0;
        DNS_DONE = false;
        for e in ARP_CACHE.iter_mut() {
            *e = EMPTY_ARP;
        }
    }
    net_unlock();
}

/// 动态应用 DHCP / 静态租约取得的配置（不重置 ARP 缓存与瞬时状态）。
pub fn set_config(ip: u32, gateway: u32, netmask: u32, dns: u32) {
    net_lock();
    unsafe {
        LOCAL_IP = ip;
        GATEWAY_IP = gateway;
        NETMASK = netmask;
        DNS_IP = dns;
    }
    net_unlock();
}

/// 本机回环地址（127.0.0.1，本地进程间通信）。
pub const LOOPBACK: u32 = 0x7F00_0001;

/// 本机 IP。
pub fn local_ip() -> u32 {
    unsafe { LOCAL_IP }
}

/// 网关 IP。
pub fn gateway() -> u32 {
    unsafe { GATEWAY_IP }
}

/// DNS 服务器 IP。
#[allow(dead_code)]
pub fn dns_ip() -> u32 {
    unsafe { DNS_IP }
}

/// 子网掩码。
#[allow(dead_code)]
pub fn netmask() -> u32 {
    unsafe { NETMASK }
}

// ==================== 以太网层 ====================

/// 组装并发送以太网帧。
fn eth_send(dst: &[u8; 6], etype: u16, payload: &[u8]) -> i32 {
    net_lock();
    let r = unsafe {
        let total = 14 + payload.len();
        if total > FRAME.len() {
            net_unlock();
            return -1;
        }
        FRAME[0..6].copy_from_slice(dst);
        FRAME[6..12].copy_from_slice(&LOCAL_MAC);
        FRAME[12] = (etype >> 8) as u8;
        FRAME[13] = (etype & 0xFF) as u8;
        FRAME[14..total].copy_from_slice(payload);
        net_virtio::send(&FRAME[..total])
    };
    net_unlock();
    r
}

// ==================== ARP ====================

fn arp_lookup(ip: u32) -> Option<[u8; 6]> {
    unsafe {
        let now = crate::timer::now_us();
        for e in ARP_CACHE.iter_mut() {
            if e.used && e.ip == ip {
                // 条目老化：超过 TTL 视为陈旧（对端可能已换 MAC/IP），
                // 清除并重新解析 —— 此前条目一旦学得就永不过期，网络
                // 拓扑变化后永远发往旧 MAC。
                if now.saturating_sub(e.age) >= ARP_ENTRY_TTL_US {
                    *e = EMPTY_ARP;
                    return None;
                }
                e.age = now; // LRU 刷新
                return Some(e.mac);
            }
        }
    }
    None
}

/// 学习一条 ARP 映射：覆盖已有项；缓存满时淘汰最旧项（简单 LRU），
/// 避免新条目因无空位而静默失败（条目过期后必须能重新学习）。
fn arp_cache_insert(ip: u32, mac: [u8; 6]) {
    if ip == 0 {
        return;
    }
    let now = crate::timer::now_us();
    unsafe {
        // 覆盖已有项
        for e in ARP_CACHE.iter_mut() {
            if e.used && e.ip == ip {
                e.mac = mac;
                e.age = now;
                return;
            }
        }
        // 新项：找空位
        for e in ARP_CACHE.iter_mut() {
            if !e.used {
                *e = ArpEntry { used: true, ip, mac, age: now };
                return;
            }
        }
        // 满：淘汰最旧（age 最小）的条目
        let mut victim = 0usize;
        let mut oldest = u64::MAX;
        for (i, e) in ARP_CACHE.iter().enumerate() {
            if e.age < oldest {
                oldest = e.age;
                victim = i;
            }
        }
        ARP_CACHE[victim] = ArpEntry { used: true, ip, mac, age: now };
    }
}

/// 发送 ARP 请求。
fn send_arp_request(ip: u32) {
    let mut a = [0u8; 28];
    a[0..2].copy_from_slice(&ARP_HTYPE_ETH.to_be_bytes());
    a[2..4].copy_from_slice(&ETH_IP.to_be_bytes());
    a[4] = 6;
    a[5] = 4;
    a[6..8].copy_from_slice(&ARP_OP_REQUEST.to_be_bytes());
    unsafe {
        a[8..14].copy_from_slice(&LOCAL_MAC);
        a[14..18].copy_from_slice(&LOCAL_IP.to_be_bytes());
    }
    a[18..24].copy_from_slice(&[0u8; 6]);
    a[24..28].copy_from_slice(&ip.to_be_bytes());
    eth_send(&[0xFF; 6], ETH_ARP, &a);
}

/// 发送 ARP 应答。
fn send_arp_reply(target_ip: u32, target_mac: [u8; 6]) {
    let mut a = [0u8; 28];
    a[0..2].copy_from_slice(&ARP_HTYPE_ETH.to_be_bytes());
    a[2..4].copy_from_slice(&ETH_IP.to_be_bytes());
    a[4] = 6;
    a[5] = 4;
    a[6..8].copy_from_slice(&ARP_OP_REPLY.to_be_bytes());
    unsafe {
        a[8..14].copy_from_slice(&LOCAL_MAC);
        a[14..18].copy_from_slice(&LOCAL_IP.to_be_bytes());
    }
    a[18..24].copy_from_slice(&target_mac);
    a[24..28].copy_from_slice(&target_ip.to_be_bytes());
    eth_send(&target_mac, ETH_ARP, &a);
}

/// 解析目标 MAC：缓存命中直接返回；否则发 ARP 请求并轮询等待应答。
fn arp_resolve(ip: u32) -> Option<[u8; 6]> {
    if let Some(m) = arp_lookup(ip) {
        return Some(m);
    }
    send_arp_request(ip);
    // 分发路径内不阻塞（避免递归），由上层后续重试
    if unsafe { IN_DISPATCH } {
        return None;
    }
    let start = crate::timer::now_us();
    let mut next_retx = start + ARP_RETX_US;
    while crate::timer::now_us() - start < ARP_TIMEOUT_US {
        poll();
        if let Some(m) = arp_lookup(ip) {
            return Some(m);
        }
        let now = crate::timer::now_us();
        if now >= next_retx {
            send_arp_request(ip);
            next_retx = now + ARP_RETX_US;
        }
        sched_cfs::yield_now();
    }
    arp_lookup(ip)
}

// ==================== IPv4 ====================

/// 组装并发送一个 IP 包（自动解析下一跳 MAC）。
///
/// 目的为 127.0.0.1 时走本地回环：直接把包注入接收路径（不经以太网/ARP），
/// 供同机进程间通信（IME 协议等）使用。
pub fn ip_send(dst_ip: u32, proto: u8, payload: &[u8]) -> i32 {
    net_lock();
    let r = unsafe {
        let total = 20 + payload.len();
        if total > IPBUF.len() {
            net_unlock();
            return -1;
        }
        let ident = next_ident();
        IPBUF[0] = 0x45; // v4 + IHL=5
        IPBUF[1] = 0;
        IPBUF[2..4].copy_from_slice(&(total as u16).to_be_bytes());
        IPBUF[4..6].copy_from_slice(&ident.to_be_bytes());
        IPBUF[6..8].copy_from_slice(&0u16.to_be_bytes());
        IPBUF[8] = 64;
        IPBUF[9] = proto;
        IPBUF[10..12].copy_from_slice(&0u16.to_be_bytes());
        IPBUF[12..16].copy_from_slice(&LOCAL_IP.to_be_bytes());
        IPBUF[16..20].copy_from_slice(&dst_ip.to_be_bytes());
        IPBUF[20..total].copy_from_slice(payload);
        let ck = checksum(&IPBUF[..20]);
        IPBUF[10..12].copy_from_slice(&ck.to_be_bytes());
        if dst_ip == LOOPBACK {
            // ★ 回环必须使用 127.0.0.1 作为源地址：此前直接注入 src=LOCAL_IP 的
            // 帧，`handle_ip` 的源过滤（src == local 一律丢弃，防网卡自环）会把
            // 回环帧整帧丢弃——本地 127.0.0.1 进程间通信永远不通。置为回环源
            // 地址并重算校验和后注入（过滤逻辑对 src==127.0.0.1 放行）。
            IPBUF[12..16].copy_from_slice(&LOOPBACK.to_be_bytes());
            let ck = checksum(&IPBUF[..20]);
            IPBUF[10..12].copy_from_slice(&ck.to_be_bytes());
            // ★ 传输层校验和的伪头含源 IP：发送端按真实 LOCAL_IP 计算，回环
            // 改写源 IP 后必须按 LOOPBACK 重算，否则接收端校验必然失败 →
            // 回环 TCP/UDP 段全部被丢弃（此前只重算 IP 头校验和）。
            fix_transport_checksum(LOOPBACK, dst_ip, proto, &mut IPBUF[20..total]);
            handle_ip(&IPBUF[..total], &[0; 6]); // 回环帧无源 MAC，学习逻辑会跳过
            net_unlock();
            return 0;
        }
        // 有限广播（255.255.255.255）：DHCP 发现阶段源地址为 0.0.0.0，无法
        // 走 ARP，直接以全 1 目标 MAC 发送（符合 RFC 2131 客户端行为）。
        if dst_ip == 0xFFFF_FFFF {
            let r = eth_send(&[0xFF; 6], ETH_IP, &IPBUF[..total]);
            net_unlock();
            return r;
        }
        let mac = match arp_resolve(next_hop(dst_ip)) {
            Some(m) => m,
            None => {
                net_unlock();
                return -1;
            }
        };
        eth_send(&mac, ETH_IP, &IPBUF[..total])
    };
    net_unlock();
    r
}

// ==================== ICMP ====================

/// 发送 ICMP echo 请求。
fn send_icmp_echo(dst: u32, seq: u16) {
    let mut msg = [0u8; 8];
    msg[0] = 8; // echo 请求
    msg[4..6].copy_from_slice(&(unsafe { ICMP_ID }).to_be_bytes());
    msg[6..8].copy_from_slice(&seq.to_be_bytes());
    let ck = checksum(&msg);
    msg[2..4].copy_from_slice(&ck.to_be_bytes());
    ip_send(dst, IPPROTO_ICMP, &msg);
}

/// ping 网关/主机：发 echo 请求并等待 echo 应答。
///
/// 整体持锁：PING_* 全局状态只属于一次会话，并发 ping 会互相应答错乱；
/// 等待有界（PING_TIMEOUT_US），期间 poll 重入、yield 让出 CPU，其他任务
/// 的联网操作在锁上睡眠退避而非死等。
pub fn ping(dst: u32) -> bool {
    net_lock();
    unsafe {
        PING_GOT_REPLY = false;
    }
    let result = if arp_resolve(dst).is_none() {
        false
    } else {
        let seq = unsafe {
            PING_SEQ = PING_SEQ.wrapping_add(1);
            PING_SEQ
        };
        send_icmp_echo(dst, seq);
        let start = crate::timer::now_us();
        let mut got = false;
        while crate::timer::now_us() - start < PING_TIMEOUT_US {
            poll();
            if unsafe { PING_GOT_REPLY } {
                got = true;
                break;
            }
            sched_cfs::yield_now();
        }
        got
    };
    net_unlock();
    result
}

/// 处理收到的 ICMP 包。
fn handle_icmp(p: &[u8], src: u32) {
    if p.len() < 8 {
        return;
    }
    // 校验整个 ICMP 报文校验和（含类型/码/校验和字段）：损坏/伪造包必须丢弃，
    // 不能既回复损坏的 echo 请求又误置 PING_GOT_REPLY。
    if checksum(p) != 0 {
        return;
    }
    let ty = p[0];
    if ty == 8 {
        // echo 请求 → 应答（先拷贝到独立缓冲，再改类型并重算校验和）
        let mut reply = [0u8; 2048];
        let n = p.len().min(reply.len());
        reply[..n].copy_from_slice(&p[..n]);
        reply[0] = 0; // echo 应答
        reply[2] = 0;
        reply[3] = 0;
        let ck = checksum(&reply[..n]);
        reply[2..4].copy_from_slice(&ck.to_be_bytes());
        ip_send(src, IPPROTO_ICMP, &reply[..n]);
    } else if ty == 0 {
        // echo 应答：必须校验 id 与 seq 都是当前 ping 会话的，
        // 防止其它主机的应答 / 伪造包误置 PING_GOT_REPLY。
        unsafe {
            let id = u16::from_be_bytes([p[4], p[5]]);
            let seq = u16::from_be_bytes([p[6], p[7]]);
            if id == ICMP_ID && seq == PING_SEQ {
                PING_GOT_REPLY = true;
            }
        }
    }
}

// ==================== UDP ====================

/// 发送 UDP 数据报。
///
/// 拒绝超过以太网 MTU 的载荷（>1472 字节会被静默截断/丢弃，返回 -1 让
/// 调用方感知失败，而非发送一个必然被网络丢弃的碎片包）。
pub fn udp_send(dst_ip: u32, sport: u16, dport: u16, data: &[u8]) -> i32 {
    if data.len() > 1472 {
        return -1;
    }
    net_lock();
    let r = unsafe {
        let total = 8 + data.len();
        if total > UDPBUF.len() {
            net_unlock();
            return -1;
        }
        UDPBUF[0..2].copy_from_slice(&sport.to_be_bytes());
        UDPBUF[2..4].copy_from_slice(&dport.to_be_bytes());
        UDPBUF[4..6].copy_from_slice(&(total as u16).to_be_bytes());
        UDPBUF[6..8].copy_from_slice(&0u16.to_be_bytes());
        UDPBUF[8..total].copy_from_slice(data);
        let ck = transport_checksum(LOCAL_IP, dst_ip, IPPROTO_UDP, &UDPBUF[..total]);
        UDPBUF[6..8].copy_from_slice(&ck.to_be_bytes());
        ip_send(dst_ip, IPPROTO_UDP, &UDPBUF[..total])
    };
    net_unlock();
    r
}

/// 处理收到的 UDP 数据报（DHCP / DNS / socket 层）。
fn handle_udp(src_ip: u32, sport: u16, dport: u16, data: &[u8]) {
    // DHCP 应答（BOOTP 68/67）：客户端在取得 IP 前使用，优先于其它分发。
    if dport == DHCP_PORT_CLI && sport == DHCP_PORT_SRV {
        dhcp_input(data);
        return;
    }
    unsafe {
        // DNS 查询应答：必须是发往我方临时端口的包、且 DNS 查询尚未完成、
        // 且源端口为标准 DNS 端口 53、且源 IP == 我方 DNS 服务器（防伪造
        // 应答；DNS_ID 与 IP IDENT 共用计数器可被预测，仅靠端口过滤不够）。
        // 解析时还会校验事务 ID。
        if DNS_SRC_PORT != 0
            && dport == DNS_SRC_PORT
            && !DNS_DONE
            && sport == DNS_PORT
            && src_ip == DNS_IP
        {
            if let Some(ip) = dns_parse_reply(data) {
                DNS_RESULT_IP = ip;
                DNS_DONE = true;
            }
            return;
        }
    }
    crate::socket::udp_input(src_ip, sport, dport, data);
}

// ==================== DNS ====================

/// 构建 DNS A 查询报文，返回长度。
fn dns_build_query(id: u16, name: &str, out: &mut [u8]) -> usize {
    out[0..2].copy_from_slice(&id.to_be_bytes());
    out[2..4].copy_from_slice(&0x0100u16.to_be_bytes()); // RD=1
    out[4..6].copy_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    out[6..8].copy_from_slice(&0u16.to_be_bytes()); // ANCOUNT
    out[8..10].copy_from_slice(&0u16.to_be_bytes()); // NSCOUNT
    out[10..12].copy_from_slice(&0u16.to_be_bytes()); // ARCOUNT
    let mut pos = 12usize;
    let mut any_label = false;
    for label in name.split('.') {
        let lb = label.as_bytes();
        // DNS label 长度上限 63 字节：超长 label（256..511 字节此前 `as u8`
        // 截断成错误长度 0/1，写出畸形查询）直接跳过。
        if lb.is_empty() || lb.len() > 63 || pos + 1 + lb.len() > out.len() {
            continue;
        }
        any_label = true;
        out[pos] = lb.len() as u8;
        pos += 1;
        out[pos..pos + lb.len()].copy_from_slice(lb);
        pos += lb.len();
    }
    // 全部 label 都放不下（超长域名等）：不能返回"只有头的空查询"——
    // 调用方会照常发出 QDCOUNT=1 而问题区为空的废包。返回 0 由调用方报错。
    if !any_label {
        return 0;
    }
    // 越界检查：终止符 + QTYPE/QCLASS（共 5 字节）必须仍在缓冲内
    if pos + 5 > out.len() {
        return 0;
    }
    out[pos] = 0;
    pos += 1;
    out[pos..pos + 2].copy_from_slice(&1u16.to_be_bytes()); // QTYPE A
    out[pos + 2..pos + 4].copy_from_slice(&1u16.to_be_bytes()); // QCLASS IN
    pos + 4
}

/// 跳过 DNS 名字（含压缩指针）。
fn skip_dns_name(data: &[u8], mut pos: usize) -> Option<usize> {
    loop {
        if pos >= data.len() {
            return None;
        }
        let len = data[pos];
        if len == 0 {
            return Some(pos + 1);
        }
        if len & 0xC0 == 0xC0 {
            return Some(pos + 2);
        }
        pos += 1 + len as usize;
    }
}

/// 从 DNS 应答中提取第一个 A 记录 IP。
fn dns_parse_reply(data: &[u8]) -> Option<u32> {
    if data.len() < 12 {
        return None;
    }
    let id = u16::from_be_bytes([data[0], data[1]]);
    if id != unsafe { DNS_ID } {
        return None;
    }
    let ancount = u16::from_be_bytes([data[6], data[7]]) as usize;
    let mut pos = 12usize;
    pos = skip_dns_name(data, pos)?;
    pos += 4; // QTYPE + QCLASS
    for _ in 0..ancount {
        pos = skip_dns_name(data, pos)?;
        if pos + 10 > data.len() {
            return None;
        }
        let ty = u16::from_be_bytes([data[pos], data[pos + 1]]);
        let rdlen = u16::from_be_bytes([data[pos + 8], data[pos + 9]]) as usize;
        pos += 10;
        if ty == 1 && rdlen == 4 && pos + 4 <= data.len() {
            return Some(u32::from_be_bytes([
                data[pos],
                data[pos + 1],
                data[pos + 2],
                data[pos + 3],
            ]));
        }
        pos += rdlen;
    }
    None
}

/// 解析域名（A 记录），失败返回 `None`。
///
/// 整体持锁：DNS_SRC_PORT/DNS_ID/DNSBUF 是全局单会话状态，并发的两次解析
/// 会互相应答错乱；等待有界（DNS_TIMEOUT_US）。
pub fn dns_resolve(name: &str) -> Option<u32> {
    net_lock();
    let dns = unsafe { DNS_IP };
    if dns == 0 || arp_resolve(dns).is_none() {
        net_unlock();
        return None;
    }
    unsafe {
        DNS_DONE = false;
        DNS_RESULT_IP = 0;
        DNS_SRC_PORT = next_port();
        DNS_ID = next_ident();
        let qlen = dns_build_query(DNS_ID, name, &mut DNSBUF);
        if qlen == 0 {
            // 域名放不进查询缓冲（build_query 静默跳过放不下的 label）：
            // 发空查询只会徒耗网络，直接失败。
            DNS_SRC_PORT = 0;
            net_unlock();
            return None;
        }
        if udp_send(dns, DNS_SRC_PORT, DNS_PORT, &DNSBUF[..qlen]) < 0 {
            DNS_SRC_PORT = 0;
            net_unlock();
            return None;
        }
    }
    let start = crate::timer::now_us();
    let mut next_retx = start + DNS_RETX_US;
    let mut result = None;
    while crate::timer::now_us() - start < DNS_TIMEOUT_US {
        poll();
        if unsafe { DNS_DONE } {
            result = Some(unsafe { DNS_RESULT_IP });
            break;
        }
        let now = crate::timer::now_us();
        if now >= next_retx {
            unsafe {
                let qlen = dns_build_query(DNS_ID, name, &mut DNSBUF);
                udp_send(dns, DNS_SRC_PORT, DNS_PORT, &DNSBUF[..qlen]);
            }
            next_retx = now + DNS_RETX_US;
        }
        sched_cfs::yield_now();
    }
    // 解析会话结束：关闭临时端口的接受窗口（否则陈旧 DNS_SRC_PORT 一直
    // 接受源端口 53 的伪应答，直到下次解析覆盖）
    unsafe {
        DNS_SRC_PORT = 0;
        DNS_DONE = false;
    }
    net_unlock();
    result
}

// ==================== DHCP 客户端（RFC 2131 极简，IPv4 动态配置） ====================
//
// 极简交互：DISCOVER →（接收 OFFER）→ REQUEST →（接收 ACK）→ 应用配置。
// 阶段状态只由 `dhcp_input`（接收）与 `dhcp_configure`（发送/轮询）修改，
// 两者均在全局网络锁内执行（dhcp_configure 整体持锁，dhcp_input 经
// poll→dispatch 到达）。超时/重发语义与 DNS 客户端一致。
//
// 触发场景：Wi-Fi 关联/握手成功后（kunxcore::wifi）调用 `dhcp_configure()`
// 从 AP（或 QEMU slirp）获得 IP/网关/掩码/DNS；有以太网时亦可复用。

// ---- DHCP 状态（BOOTP 字段与选项常量） ----
const BOOTP_OP_REQUEST: u8 = 1;
const BOOTP_HW_ETHERNET: u8 = 1;
const BOOTP_HLEN_ETHERNET: u8 = 6;
const DHCP_MAGIC: [u8; 4] = [0x63, 0x82, 0x53, 0x63];

const DHCP_MSG_DISCOVER: u8 = 1;
const DHCP_MSG_OFFER: u8 = 2;
const DHCP_MSG_REQUEST: u8 = 3;
const DHCP_MSG_ACK: u8 = 5;
const DHCP_MSG_NAK: u8 = 6;

const DHCP_OPT_MSG_TYPE: u8 = 53;
const DHCP_OPT_SERVER_ID: u8 = 54;
const DHCP_OPT_REQUESTED_IP: u8 = 50;
const DHCP_OPT_SUBNET_MASK: u8 = 1;
const DHCP_OPT_ROUTER: u8 = 3;
const DHCP_OPT_DNS: u8 = 6;
const DHCP_OPT_END: u8 = 255;

static mut DHCP_TXID: u32 = 0x2026_0000;
static mut DHCP_PHASE: u8 = 0; // 0=空闲 1=等待 OFFER 2=等待 ACK
static mut DHCP_DONE: bool = false;
static mut DHCP_FAIL: bool = false;
static mut DHCP_OFFER_IP: u32 = 0;
static mut DHCP_OFFER_MASK: u32 = 0xFFFF_FF00;
static mut DHCP_OFFER_GW: u32 = 0;
static mut DHCP_OFFER_DNS: u32 = 0;
static mut DHCP_OFFER_SERVER: u32 = 0;

/// 组装 BOOTP/DHCP 报文（RFC 2131 图 2 固定字段 + 选项），返回字节数。
fn dhcp_build(ty: u8, out: &mut [u8]) -> usize {
    if out.len() < 240 {
        return 0;
    }
    out.fill(0);
    out[0] = BOOTP_OP_REQUEST; // op = BOOTREQUEST
    out[1] = BOOTP_HW_ETHERNET; // htype
    out[2] = BOOTP_HLEN_ETHERNET; // hlen
    out[3] = 0; // hops
    out[4..8].copy_from_slice(&(unsafe { DHCP_TXID }).to_be_bytes()); // xid
    out[8..10].copy_from_slice(&0u16.to_be_bytes()); // secs
    out[10..12].copy_from_slice(&0x8000u16.to_be_bytes()); // flags: 广播位
    // ciaddr / yiaddr / siaddr / giaddr 均为 0
    unsafe {
        out[28..34].copy_from_slice(&LOCAL_MAC);
    }
    out[236..240].copy_from_slice(&DHCP_MAGIC); // cookie
    let mut pos = 240usize;
    // 选项：消息类型
    out[pos] = DHCP_OPT_MSG_TYPE;
    out[pos + 1] = 1;
    out[pos + 2] = ty;
    pos += 3;
    if ty == DHCP_MSG_REQUEST {
        // 请求阶段：请求 OFFER 给出的地址并指定服务器
        out[pos] = DHCP_OPT_REQUESTED_IP;
        out[pos + 1] = 4;
        out[pos + 2..pos + 6].copy_from_slice(&(unsafe { DHCP_OFFER_IP }).to_be_bytes());
        pos += 6;
        out[pos] = DHCP_OPT_SERVER_ID;
        out[pos + 1] = 4;
        out[pos + 2..pos + 6].copy_from_slice(&(unsafe { DHCP_OFFER_SERVER }).to_be_bytes());
        pos += 6;
    }
    out[pos] = DHCP_OPT_END;
    pos + 1
}

/// 遍历 DHCP 选项，按 code 回调（body 为选项值）。
fn dhcp_parse_options(mut data: &[u8], mut f: impl FnMut(u8, &[u8])) {
    while data.len() >= 2 {
        let code = data[0];
        let len = data[1] as usize;
        if code == DHCP_OPT_END {
            break;
        }
        if code == 0 {
            data = &data[1..]; // pad
            continue;
        }
        if 2 + len > data.len() {
            break;
        }
        f(code, &data[2..2 + len]);
        data = &data[2 + len..];
    }
}

/// 收到 DHCP 应答（OFFER/ACK/NAK）时的处理。
fn dhcp_input(data: &[u8]) {
    unsafe {
        // 固定头校验：op=BOOTREPLY、cookie、事务 ID 一致
        if data.len() < 240
            || data[0] != 2
            || data[236..240] != DHCP_MAGIC
            || u32::from_be_bytes([data[4], data[5], data[6], data[7]]) != DHCP_TXID
        {
            return;
        }
        let yiaddr = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
        let mut msg_ty = 0u8;
        let mut server = 0u32;
        let mut mask = 0xFFFF_FF00u32;
        let mut gw = 0u32;
        let mut dns = 0u32;
        dhcp_parse_options(&data[240..], &mut |code: u8, body: &[u8]| match code {
            DHCP_OPT_MSG_TYPE if body.len() >= 1 => msg_ty = body[0],
            DHCP_OPT_SERVER_ID if body.len() >= 4 => {
                server = u32::from_be_bytes([body[0], body[1], body[2], body[3]])
            }
            DHCP_OPT_SUBNET_MASK if body.len() >= 4 => {
                mask = u32::from_be_bytes([body[0], body[1], body[2], body[3]])
            }
            DHCP_OPT_ROUTER if body.len() >= 4 => {
                gw = u32::from_be_bytes([body[0], body[1], body[2], body[3]])
            }
            DHCP_OPT_DNS if body.len() >= 4 => {
                dns = u32::from_be_bytes([body[0], body[1], body[2], body[3]])
            }
            _ => {}
        });
        match (msg_ty, DHCP_PHASE) {
            (DHCP_MSG_OFFER, 1) => {
                // 缓存 OFFER，进入 REQUEST 阶段
                DHCP_OFFER_IP = yiaddr;
                DHCP_OFFER_SERVER = server;
                DHCP_OFFER_MASK = mask;
                DHCP_OFFER_GW = gw;
                DHCP_OFFER_DNS = dns;
                DHCP_PHASE = 2;
            }
            (DHCP_MSG_ACK, 2) => {
                // ACK：应用租约配置
                LOCAL_IP = yiaddr;
                NETMASK = mask;
                GATEWAY_IP = gw;
                DNS_IP = dns;
                DHCP_DONE = true;
            }
            (DHCP_MSG_NAK, 2) => {
                DHCP_FAIL = true;
            }
            _ => {}
        }
    }
}

/// 动态取得 IP 配置（DISCOVER → REQUEST → ACK），成功返回 `true`。
///
/// 成功后本机 IP/掩码/网关/DNS 已应用（与 `ifconfig`/网络栈生效）。失败
/// （无响应 / NAK）返回 `false`，网络栈保持原配置。超时约 15s，期间每 2s
/// 重发当前阶段报文。
pub fn dhcp_configure() -> bool {
    net_lock();
    unsafe {
        DHCP_TXID = DHCP_TXID.wrapping_add(1);
        if DHCP_TXID == 0 {
            DHCP_TXID = 0x2026_0001;
        }
        DHCP_PHASE = 1;
        DHCP_DONE = false;
        DHCP_FAIL = false;
        DHCP_OFFER_IP = 0;
        DHCP_OFFER_SERVER = 0;
    }
    let start = crate::timer::now_us();
    let mut next_retx = start;
    let mut result = false;
    while crate::timer::now_us() - start < DHCP_TIMEOUT_US {
        unsafe {
            if DHCP_DONE {
                result = true;
                break;
            }
            if DHCP_FAIL {
                break;
            }
        }
        let now = crate::timer::now_us();
        let phase = unsafe { DHCP_PHASE };
        if now >= next_retx {
            let ty = if phase == 2 { DHCP_MSG_REQUEST } else { DHCP_MSG_DISCOVER };
            let qlen = unsafe { dhcp_build(ty, &mut DNSBUF) }; // 复用 DNS 缓冲（同长度级）
            unsafe { udp_send(0xFFFF_FFFF, DHCP_PORT_CLI, DHCP_PORT_SRV, &DNSBUF[..qlen]); }
            next_retx = now + DHCP_RETX_US;
        }
        poll();
        sched_cfs::yield_now();
    }
    net_unlock();
    result
}

// ==================== 分发 ====================

fn handle_arp(p: &[u8]) {
    if p.len() < 28 {
        return;
    }
    // 校验协议字段：以太网 + IPv4 + 6/4 字节地址，防止格式错误/伪造包污染缓存
    let htype = u16::from_be_bytes([p[0], p[1]]);
    let ptype = u16::from_be_bytes([p[2], p[3]]);
    if htype != ARP_HTYPE_ETH || ptype != ETH_IP || p[4] != 6 || p[5] != 4 {
        return;
    }
    let oper = u16::from_be_bytes([p[6], p[7]]);
    if oper != ARP_OP_REQUEST && oper != ARP_OP_REPLY {
        return;
    }
    let sender_mac = [p[8], p[9], p[10], p[11], p[12], p[13]];
    let sender_ip = u32::from_be_bytes([p[14], p[15], p[16], p[17]]);
    // 学习发送方映射（拒绝 0.0.0.0 与自身地址，避免缓存被垃圾条目占满）
    if sender_ip != 0 && sender_ip != unsafe { LOCAL_IP } {
        arp_cache_insert(sender_ip, sender_mac);
    }

    if oper == ARP_OP_REQUEST {
        let target_ip = u32::from_be_bytes([p[24], p[25], p[26], p[27]]);
        if target_ip == unsafe { LOCAL_IP } {
            send_arp_reply(sender_ip, sender_mac);
        }
    }
}

fn handle_ip(p: &[u8], src_mac: &[u8]) {
    if p.len() < 20 {
        return;
    }
    let ihl = ((p[0] & 0x0F) as usize) * 4;
    if ihl < 20 || p.len() < ihl {
        return;
    }
    // 用 IP 总长度字段截断，避免把以太网最小帧的填充字节当成载荷
    let total_len = u16::from_be_bytes([p[2], p[3]]) as usize;
    if total_len < ihl || total_len > p.len() {
        return;
    }
    // 分片处理：本内核不实现重组，MF 置位或片偏移非 0 的分片包直接丢弃，
    // 避免把非首片当独立 TCP/UDP 段解析造成连接状态错乱。
    let frag = u16::from_be_bytes([p[6], p[7]]);
    if frag & 0x2000 != 0 || frag & 0x1FFF != 0 {
        return;
    }
    let proto = p[9];
    let src = u32::from_be_bytes([p[12], p[13], p[14], p[15]]);
    let dst = u32::from_be_bytes([p[16], p[17], p[18], p[19]]);
    let local = unsafe { LOCAL_IP };
    // 源地址过滤：本机地址不会从网卡收到（防自环注入）；0.0.0.0 仅在 DHCP
    // 阶段（LOCAL_IP==0，未取得 IP）合法（RFC 2131），否则直接丢弃。
    if src == local || (src == 0 && local != 0) {
        return;
    }
    // 目的地址过滤：只处理发给本机、广播或回环的包；0.0.0.0 仅当本机未取得
    // IP（DHCP 阶段，RFC 2131 允许服务器发往 0.0.0.0）时接受，否则丢弃。
    if dst != local
        && dst != 0xFFFF_FFFF
        && dst != LOOPBACK
        && !(dst == 0 && local == 0)
    {
        return;
    }
    let payload = &p[ihl..total_len];

    // 校验 IP 头校验和（防传输损坏的包被当作有效数据解析）
    if checksum(&p[..ihl]) != 0 {
        return;
    }

    // ARP 主动学习：能通过上述过滤的数据帧其发送方 MAC/IP 可信，若该地址
    // 尚未缓存则录入。否则 ACK/SYN-ACK 发出前 arp_resolve 在分发路径内
    //（IN_DISPATCH）不能发请求等待，只能得到 None → 应答被丢。
    // ★ 排除回环地址（127/8）：ip_send(127.0.0.1) 本地注入路径 src_mac 全零，
    // 学习会污染 ARP 缓存（8 项 LRU 挤掉真实网关条目）。
    if src != 0
        && src != local
        && (src & 0xFF00_0000) != 0x7F00_0000
        && src_mac.len() >= 6
        && arp_lookup(src).is_none()
    {
        let mac = [src_mac[0], src_mac[1], src_mac[2], src_mac[3], src_mac[4], src_mac[5]];
        arp_cache_insert(src, mac);
    }

    match proto {
        IPPROTO_ICMP => handle_icmp(payload, src),
        IPPROTO_UDP => {
            if payload.len() >= 8 {
                let ulen = u16::from_be_bytes([payload[4], payload[5]]) as usize;
                let sport = u16::from_be_bytes([payload[0], payload[1]]);
                let dport = u16::from_be_bytes([payload[2], payload[3]]);
                let end = ulen.min(payload.len());
                if end >= 8 {
                    // UDP 校验和：0 表示发送方未计算（IPv4 允许），其余必须通过
                    //（含伪头，正确包合计为 0）
                    let ck = u16::from_be_bytes([payload[6], payload[7]]);
                    if ck != 0 && transport_checksum(src, dst, IPPROTO_UDP, &payload[..end]) != 0 {
                        return; // 校验失败：丢弃
                    }
                    handle_udp(src, sport, dport, &payload[8..end]);
                }
            }
        }
        IPPROTO_TCP => {
            // TCP 校验和必须通过（含伪头），防损坏段破坏连接状态；
            // 校验和为 0 也是非法：IPv4 TCP 不允许省略校验和（0 仅 IPv4 UDP 合法）。
            if payload.len() >= 20 {
                let ck = u16::from_be_bytes([payload[16], payload[17]]);
                if ck == 0 || transport_checksum(src, dst, IPPROTO_TCP, payload) != 0 {
                    return;
                }
            }
            crate::socket::tcp_input(src, payload)
        }
        _ => {}
    }
}

fn dispatch(frame: &[u8]) {
    if frame.len() < 14 {
        return;
    }
    let etype = u16::from_be_bytes([frame[12], frame[13]]);
    match etype {
        ETH_ARP => handle_arp(&frame[14..]),
        // 传入以太网源 MAC 供 handle_ip 做 ARP 主动学习
        ETH_IP => handle_ip(&frame[14..], &frame[6..12]),
        _ => {}
    }
}

/// 轮询一次：回收硬件环，分发软件接收队列中的所有帧。
///
/// 整体持锁：RXFRAME 与全部 input 分发路径（含回注发送）都在锁内；调用方
/// 已持锁时靠 tid 重入放行。分发过程中无阻塞等待（input 只入队/应答），
/// 持锁时长与排队帧数成正比。
pub fn poll() {
    net_lock();
    net_virtio::drain();
    unsafe {
        IN_DISPATCH = true;
    }
    loop {
        let n = unsafe { net_virtio::recv_into(&mut RXFRAME) };
        match n {
            Some(len) => dispatch(unsafe { &RXFRAME[..len] }),
            None => break,
        }
    }
    unsafe {
        IN_DISPATCH = false;
    }
    net_unlock();
}
