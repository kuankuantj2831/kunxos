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
//! 把硬件环里的包搬进软件接收队列（见 `net_virtio`），因此无需跨上下文锁。

use crate::net_virtio;
use crate::sched_cfs;

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

const ARP_MAX: usize = 8;
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
static mut CHECKBUF: [u8; 2048] = [0; 2048]; // 传输层伪头校验和
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
    unsafe {
        IDENT = IDENT.wrapping_add(1);
        IDENT
    }
}

/// 分配临时源端口（socket 层用）。
pub fn next_port() -> u16 {
    unsafe {
        EPHEMERAL = EPHEMERAL.wrapping_add(1);
        if EPHEMERAL < 0x4000 {
            EPHEMERAL = 0x4000;
        }
        EPHEMERAL
    }
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
    arp_resolve(next_hop(ip)).is_some()
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

// ==================== 初始化 ====================

/// 初始化协议栈配置。
pub fn init(mac: [u8; 6], ip: u32, gateway: u32, netmask: u32, dns: u32) {
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
}

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
    unsafe {
        let total = 14 + payload.len();
        if total > FRAME.len() {
            return -1;
        }
        FRAME[0..6].copy_from_slice(dst);
        FRAME[6..12].copy_from_slice(&LOCAL_MAC);
        FRAME[12] = (etype >> 8) as u8;
        FRAME[13] = (etype & 0xFF) as u8;
        FRAME[14..total].copy_from_slice(payload);
        net_virtio::send(&FRAME[..total])
    }
}

// ==================== ARP ====================

fn arp_lookup(ip: u32) -> Option<[u8; 6]> {
    unsafe {
        for e in ARP_CACHE.iter_mut() {
            if e.used && e.ip == ip {
                e.age = crate::timer::now_us(); // LRU 刷新
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
pub fn ip_send(dst_ip: u32, proto: u8, payload: &[u8]) -> i32 {
    let mac = match arp_resolve(next_hop(dst_ip)) {
        Some(m) => m,
        None => return -1,
    };
    unsafe {
        let total = 20 + payload.len();
        if total > IPBUF.len() {
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
        eth_send(&mac, ETH_IP, &IPBUF[..total])
    }
}

// ==================== ICMP ====================

/// 发送 ICMP echo 请求。
fn send_icmp_echo(dst: u32, seq: u16) {
    let mut msg = [0u8; 8];
    msg[0] = 8; // echo request
    msg[4..6].copy_from_slice(&(unsafe { ICMP_ID }).to_be_bytes());
    msg[6..8].copy_from_slice(&seq.to_be_bytes());
    let ck = checksum(&msg);
    msg[2..4].copy_from_slice(&ck.to_be_bytes());
    ip_send(dst, IPPROTO_ICMP, &msg);
}

/// ping 网关/主机：发 echo 请求并等待 echo 应答。
pub fn ping(dst: u32) -> bool {
    unsafe {
        PING_GOT_REPLY = false;
    }
    if arp_resolve(dst).is_none() {
        return false;
    }
    let seq = unsafe {
        PING_SEQ = PING_SEQ.wrapping_add(1);
        PING_SEQ
    };
    send_icmp_echo(dst, seq);
    let start = crate::timer::now_us();
    while crate::timer::now_us() - start < PING_TIMEOUT_US {
        poll();
        unsafe {
            if PING_GOT_REPLY {
                return true;
            }
        }
        sched_cfs::yield_now();
    }
    false
}

/// 处理收到的 ICMP 包。
fn handle_icmp(p: &[u8], src: u32) {
    if p.len() < 8 {
        return;
    }
    let ty = p[0];
    if ty == 8 {
        // echo request → reply（先拷贝到独立缓冲，再改类型并重算校验和）
        let mut reply = [0u8; 2048];
        let n = p.len().min(reply.len());
        reply[..n].copy_from_slice(&p[..n]);
        reply[0] = 0; // echo reply
        reply[2] = 0;
        reply[3] = 0;
        let ck = checksum(&reply[..n]);
        reply[2..4].copy_from_slice(&ck.to_be_bytes());
        ip_send(src, IPPROTO_ICMP, &reply[..n]);
    } else if ty == 0 {
        unsafe {
            PING_GOT_REPLY = true;
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
    unsafe {
        let total = 8 + data.len();
        if total > UDPBUF.len() {
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
    }
}

/// 处理收到的 UDP 数据报（DNS 或 socket 层）。
fn handle_udp(src_ip: u32, sport: u16, dport: u16, data: &[u8]) {
    unsafe {
        // DNS 查询应答：必须是发往我方临时端口的包、且 DNS 查询尚未完成、
        // 且源端口为标准 DNS 端口 53（防伪造应答）；解析时还会校验事务 ID。
        if DNS_SRC_PORT != 0
            && dport == DNS_SRC_PORT
            && !DNS_DONE
            && sport == DNS_PORT
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
    for label in name.split('.') {
        let lb = label.as_bytes();
        // DNS label 长度上限 63 字节：超长 label（256..511 字节此前 `as u8`
        // 截断成错误长度 0/1，写出畸形查询）直接跳过。
        if lb.is_empty() || lb.len() > 63 || pos + 1 + lb.len() > out.len() {
            continue;
        }
        out[pos] = lb.len() as u8;
        pos += 1;
        out[pos..pos + lb.len()].copy_from_slice(lb);
        pos += lb.len();
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
pub fn dns_resolve(name: &str) -> Option<u32> {
    let dns = unsafe { DNS_IP };
    if dns == 0 || arp_resolve(dns).is_none() {
        return None;
    }
    unsafe {
        DNS_DONE = false;
        DNS_RESULT_IP = 0;
        DNS_SRC_PORT = next_port();
        DNS_ID = next_ident();
        let qlen = dns_build_query(DNS_ID, name, &mut DNSBUF);
        if udp_send(dns, DNS_SRC_PORT, DNS_PORT, &DNSBUF[..qlen]) < 0 {
            return None;
        }
    }
    let start = crate::timer::now_us();
    let mut next_retx = start + DNS_RETX_US;
    while crate::timer::now_us() - start < DNS_TIMEOUT_US {
        poll();
        unsafe {
            if DNS_DONE {
                return Some(DNS_RESULT_IP);
            }
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
    None
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

fn handle_ip(p: &[u8]) {
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
    // 目的地址过滤：只处理发给本机或广播的包（忽略共享网络里发给其它主机的包）
    if dst != unsafe { LOCAL_IP } && dst != 0xFFFF_FFFF {
        return;
    }
    let payload = &p[ihl..total_len];

    // 校验 IP 头校验和（防传输损坏的包被当作有效数据解析）
    if checksum(&p[..ihl]) != 0 {
        return;
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
            // TCP 校验和必须通过（含伪头），防损坏段破坏连接状态
            if payload.len() >= 20 {
                let ck = u16::from_be_bytes([payload[16], payload[17]]);
                if ck != 0 && transport_checksum(src, dst, IPPROTO_TCP, payload) != 0 {
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
        ETH_IP => handle_ip(&frame[14..]),
        _ => {}
    }
}

/// 轮询一次：回收硬件环，分发软件接收队列中的所有帧。
pub fn poll() {
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
}
