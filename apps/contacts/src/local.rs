//! 本地联系人存储与 vCard 导入 / 导出（no_std、无堆、裸 syscall）。
//!
//! 设计：
//!   - **存储**：`/data/contacts/contacts.local` 单文件文本格式，一行一个联系人：
//!     `<id>\x1F<姓名>\x1F<电话>\x1F<邮箱>\x1F<地址>\n`。字段分隔 0x1F（同旧
//!     contacts.dat 惯例）、记录分隔 `\n`，输入自动剔除分隔符。
//!   - **CRUD**：增删改查，静态数组承载（≤ `MAX_LOCAL` 个），O(n) 遍历。
//!   - **搜索**：姓名 / 电话前缀匹配（ASCII 大小写不敏感，纯文本，不绑拼音库）。
//!   - **vCard 导入**：手写状态机只读 `BEGIN/END:VCARD`、`FN`、`TEL`、`EMAIL`
//!     （简化实现，支持折叠续行与基本转义）。
//!   - **vCard 导出**：拼接 vCard 3.0 文本（FN / TEL / EMAIL / ADR）。

use crate::kit::*;

// ==================== 常量 ====================

const SYS_MKDIRAT: usize = 34;
const O_CREAT: usize = 0x40;
const O_TRUNC: usize = 0x200;

pub const LOCAL_DIR: &str = "/data/contacts";
pub const LOCAL_PATH: &str = "/data/contacts/contacts.local";
pub const IMPORT_VCARD_PATH: &str = "/data/contacts/import.vcf";
pub const EXPORT_VCARD_PATH: &str = "/data/contacts/export.vcf";

/// 本地联系人上限（任务约束：< 1000；与同步存储 MAX_RECORDS=512 对齐）。
pub const MAX_LOCAL: usize = 512;

pub const NAME_CAP: usize = 48;
pub const PHONE_CAP: usize = 32;
pub const EMAIL_CAP: usize = 64;
pub const ADDR_CAP: usize = 96;

/// 单条本地联系人记录（Copy，规避静态借用）。
#[derive(Clone, Copy)]
pub struct Rec {
    pub id: u32,
    pub name: [u8; NAME_CAP],
    pub name_len: usize,
    pub phone: [u8; PHONE_CAP],
    pub phone_len: usize,
    pub email: [u8; EMAIL_CAP],
    pub email_len: usize,
    pub addr: [u8; ADDR_CAP],
    pub addr_len: usize,
}

// ==================== 全局状态 ====================

static mut RECS: [Rec; MAX_LOCAL] = [Rec {
    id: 0,
    name: [0; NAME_CAP],
    name_len: 0,
    phone: [0; PHONE_CAP],
    phone_len: 0,
    email: [0; EMAIL_CAP],
    email_len: 0,
    addr: [0; ADDR_CAP],
    addr_len: 0,
}; MAX_LOCAL];
static mut RECS_N: usize = 0;
static mut NEXT_ID: u32 = 1;

/// 读写共用缓冲（最大记录行 ≈ 10+48+32+64+96+5 = 255 字节 × 512 ≈ 130KB）。
static mut FILE_BUF: [u8; 131072] = [0u8; 131072];

// ==================== 基础 I/O ====================

fn read_file(path: &str, buf: &mut [u8]) -> isize {
    unsafe {
        let fd = open(path, O_RDONLY);
        if fd < 0 {
            return -1;
        }
        let n = syscall3(SYS_READ, fd as usize, buf.as_mut_ptr() as usize, buf.len());
        syscall1(SYS_CLOSE, fd as usize);
        n as isize
    }
}

fn write_file(path: &str, data: &[u8]) -> bool {
    unsafe {
        let fd = open(path, O_WRONLY | O_CREAT | O_TRUNC);
        if fd < 0 {
            return false;
        }
        // 分片写循环：FILE_BUF 可达 ~130KB，单次 write 可能短写 —— 之前
        // 只写一次且调用方丢弃返回值，短写即静默丢数据且文件已被截断。
        let mut off = 0usize;
        let mut ok = true;
        while off < data.len() {
            let n = syscall3(
                SYS_WRITE,
                fd as usize,
                data[off..].as_ptr() as usize,
                data.len() - off,
            ) as isize;
            if n <= 0 {
                ok = false;
                break;
            }
            off += n as usize;
        }
        syscall1(SYS_CLOSE, fd as usize);
        ok
    }
}

/// 确保本地目录存在（已存在则忽略失败）。
pub fn ensure_dir() {
    let mut p = [0u8; 64];
    let n = copy_into(&mut p, LOCAL_DIR.as_bytes()).min(p.len() - 1);
    p[n] = 0;
    unsafe {
        syscall3(SYS_MKDIRAT, AT_FDCWD, p.as_ptr() as usize, 0o755);
    }
}

// ==================== 缓冲/字符串辅助 ====================

fn copy_into(dst: &mut [u8], src: &[u8]) -> usize {
    let n = src.len().min(dst.len());
    dst[..n].copy_from_slice(&src[..n]);
    n
}

fn push_slice(buf: &mut [u8], at: usize, s: &[u8]) -> usize {
    let room = buf.len().saturating_sub(at);
    let n = s.len().min(room);
    if n > 0 {
        buf[at..at + n].copy_from_slice(&s[..n]);
    }
    at + n
}

fn push_dec(buf: &mut [u8], at: usize, mut v: u64) -> usize {
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    if v == 0 {
        i -= 1;
        tmp[i] = b'0';
    } else {
        while v > 0 {
            i -= 1;
            tmp[i] = b'0' + (v % 10) as u8;
            v /= 10;
        }
    }
    push_slice(buf, at, &tmp[i..])
}

/// 剔除会破坏行格式的分隔符（0x1F / \r\n），并截断到目标容量。
fn sanitize(src: &[u8], dst: &mut [u8]) -> usize {
    let mut n = 0usize;
    for &b in src {
        if b == 0x1F || b == b'\n' || b == b'\r' || b == 0 {
            continue;
        }
        if n >= dst.len() {
            break;
        }
        dst[n] = b;
        n += 1;
    }
    n
}

fn trim_ascii(s: &[u8]) -> &[u8] {
    let mut a = 0usize;
    let mut b = s.len();
    while a < b && (s[a] == b' ' || s[a] == b'\t' || s[a] == b'\r' || s[a] == b'\n') {
        a += 1;
    }
    while b > a && (s[b - 1] == b' ' || s[b - 1] == b'\t' || s[b - 1] == b'\r' || s[b - 1] == b'\n') {
        b -= 1;
    }
    &s[a..b]
}

fn eq_ignore_case(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.eq_ignore_ascii_case(b)
}

fn starts_with_ci(a: &[u8], p: &[u8]) -> bool {
    a.len() >= p.len() && a[..p.len()].eq_ignore_ascii_case(p)
}

/// vCard 属性行切分：`NAME[;参数]:值` → (属性名, 值)。
fn split_prop(line: &[u8]) -> (&[u8], Option<&[u8]>) {
    let colon = match line.iter().position(|&b| b == b':') {
        Some(c) => c,
        None => return (line, None),
    };
    let semi = line.iter().position(|&b| b == b';');
    let name_end = match semi {
        Some(s) if s < colon => s,
        _ => colon,
    };
    (&line[..name_end], Some(&line[colon + 1..]))
}

/// 去除 vCard 转义（`\n`/`\,`/`\;`/`\\`）。
fn unescape(src: &[u8], dst: &mut [u8]) -> usize {
    let mut n = 0usize;
    let mut i = 0usize;
    while i < src.len() {
        if n >= dst.len() {
            break;
        }
        if src[i] == b'\\' && i + 1 < src.len() {
            match src[i + 1] {
                b'n' | b'N' => {
                    dst[n] = b'\n';
                    n += 1;
                    i += 2;
                }
                b',' => {
                    dst[n] = b',';
                    n += 1;
                    i += 2;
                }
                b';' => {
                    dst[n] = b';';
                    n += 1;
                    i += 2;
                }
                b'\\' => {
                    dst[n] = b'\\';
                    n += 1;
                    i += 2;
                }
                _ => {
                    dst[n] = src[i];
                    n += 1;
                    i += 1;
                }
            }
        } else {
            dst[n] = src[i];
            n += 1;
            i += 1;
        }
    }
    n
}

/// 写入 vCard 转义（`\r\n`→`\n`，`,` / `;` / `\` 前缀反斜杠）。
fn escape(src: &[u8], dst: &mut [u8]) -> usize {
    let mut n = 0usize;
    for &c in src {
        if n + 2 > dst.len() {
            break;
        }
        match c {
            b'\n' => {
                dst[n] = b'\\';
                dst[n + 1] = b'n';
                n += 2;
            }
            b',' => {
                dst[n] = b'\\';
                dst[n + 1] = b',';
                n += 2;
            }
            b';' => {
                dst[n] = b'\\';
                dst[n + 1] = b';';
                n += 2;
            }
            b'\\' => {
                dst[n] = b'\\';
                dst[n + 1] = b'\\';
                n += 2;
            }
            other => {
                dst[n] = other;
                n += 1;
            }
        }
    }
    n
}

// ==================== 增删改查（CRUD） ====================

fn parse_id(s: &[u8]) -> u32 {
    let mut v: u32 = 0;
    for &c in s {
        if c < b'0' || c > b'9' {
            return 0;
        }
        v = v.wrapping_mul(10).wrapping_add((c - b'0') as u32);
    }
    v
}

/// 从磁盘载入；返回联系人数量（0 = 空或不存在）。
pub fn load() -> usize {
    unsafe {
        let n = read_file(LOCAL_PATH, &mut FILE_BUF);
        RECS_N = 0;
        NEXT_ID = 1;
        if n <= 0 {
            return 0;
        }
        let n = n as usize;
        let mut at = 0usize;
        while at < n && RECS_N < MAX_LOCAL {
            let eol = match FILE_BUF[at..n].iter().position(|&b| b == b'\n') {
                Some(x) => at + x,
                None => n,
            };
            let mut line = &FILE_BUF[at..eol];
            if line.len() > 0 && line[line.len() - 1] == b'\r' {
                line = &line[..line.len() - 1];
            }
            parse_line(line);
            at = eol + 1;
        }
        RECS_N
    }
}

fn parse_line(line: &[u8]) {
    if line.len() == 0 || line[0] == b'#' {
        return;
    }
    unsafe {
        if RECS_N >= MAX_LOCAL {
            return;
        }
        let mut parts: [&[u8]; 5] = [&[]; 5];
        let mut pn = 0usize;
        let mut rest = line;
        while pn < 5 {
            match rest.iter().position(|&b| b == 0x1F) {
                Some(x) => {
                    parts[pn] = &rest[..x];
                    pn += 1;
                    rest = &rest[x + 1..];
                }
                None => {
                    parts[pn] = rest;
                    pn += 1;
                    break;
                }
            }
        }
        let mut r = Rec {
            id: parse_id(parts[0]),
            ..Rec {
                id: 0,
                name: [0; NAME_CAP],
                name_len: 0,
                phone: [0; PHONE_CAP],
                phone_len: 0,
                email: [0; EMAIL_CAP],
                email_len: 0,
                addr: [0; ADDR_CAP],
                addr_len: 0,
            }
        };
        if r.id == 0 {
            return;
        }
        if pn >= 2 {
            r.name_len = sanitize(parts[1], &mut r.name);
        }
        if pn >= 3 {
            r.phone_len = sanitize(parts[2], &mut r.phone);
        }
        if pn >= 4 {
            r.email_len = sanitize(parts[3], &mut r.email);
        }
        if pn >= 5 {
            r.addr_len = sanitize(parts[4], &mut r.addr);
        }
        if r.id >= NEXT_ID {
            NEXT_ID = r.id.wrapping_add(1);
        }
        RECS[RECS_N] = r;
        RECS_N += 1;
    }
}

/// 全量写回磁盘。
pub fn save() -> bool {
    unsafe {
        let mut n = 0usize;
        for i in 0..RECS_N {
            let r = RECS[i];
            n = push_dec(&mut FILE_BUF, n, r.id as u64);
            n = push_slice(&mut FILE_BUF, n, b"\x1F");
            n = push_slice(&mut FILE_BUF, n, &r.name[..r.name_len]);
            n = push_slice(&mut FILE_BUF, n, b"\x1F");
            n = push_slice(&mut FILE_BUF, n, &r.phone[..r.phone_len]);
            n = push_slice(&mut FILE_BUF, n, b"\x1F");
            n = push_slice(&mut FILE_BUF, n, &r.email[..r.email_len]);
            n = push_slice(&mut FILE_BUF, n, b"\x1F");
            n = push_slice(&mut FILE_BUF, n, &r.addr[..r.addr_len]);
            n = push_slice(&mut FILE_BUF, n, b"\n");
        }
        write_file(LOCAL_PATH, &FILE_BUF[..n])
    }
}

pub fn count() -> usize {
    unsafe { RECS_N }
}

/// 按下标取记录（Copy）。
pub fn get(i: usize) -> Option<Rec> {
    unsafe { if i < RECS_N { Some(RECS[i]) } else { None } }
}

pub fn index_of(id: u32) -> Option<usize> {
    unsafe {
        for i in 0..RECS_N {
            if RECS[i].id == id {
                return Some(i);
            }
        }
        None
    }
}

/// 新增；返回 id（0 = 全部为空或已满）。
pub fn add(name: &[u8], phone: &[u8], email: &[u8], addr: &[u8]) -> u32 {
    unsafe {
        if RECS_N >= MAX_LOCAL {
            return 0;
        }
        let mut r = Rec {
            id: NEXT_ID,
            name: [0; NAME_CAP],
            name_len: 0,
            phone: [0; PHONE_CAP],
            phone_len: 0,
            email: [0; EMAIL_CAP],
            email_len: 0,
            addr: [0; ADDR_CAP],
            addr_len: 0,
        };
        r.name_len = sanitize(name, &mut r.name);
        r.phone_len = sanitize(phone, &mut r.phone);
        r.email_len = sanitize(email, &mut r.email);
        r.addr_len = sanitize(addr, &mut r.addr);
        if r.name_len == 0 && r.phone_len == 0 && r.email_len == 0 && r.addr_len == 0 {
            return 0;
        }
        let id = NEXT_ID;
        NEXT_ID = NEXT_ID.wrapping_add(1);
        RECS[RECS_N] = r;
        RECS_N += 1;
        id
    }
}

/// 按 id 更新；返回是否找到。
pub fn update(id: u32, name: &[u8], phone: &[u8], email: &[u8], addr: &[u8]) -> bool {
    unsafe {
        let i = match index_of(id) {
            Some(i) => i,
            None => return false,
        };
        let mut r = RECS[i];
        r.name_len = sanitize(name, &mut r.name);
        r.phone_len = sanitize(phone, &mut r.phone);
        r.email_len = sanitize(email, &mut r.email);
        r.addr_len = sanitize(addr, &mut r.addr);
        RECS[i] = r;
        true
    }
}

/// 按 id 删除（尾部前移）。
pub fn delete(id: u32) -> bool {
    unsafe {
        let i = match index_of(id) {
            Some(i) => i,
            None => return false,
        };
        for k in i..RECS_N - 1 {
            RECS[k] = RECS[k + 1];
        }
        RECS_N -= 1;
        true
    }
}

/// 按电话精确查找（去重用）。
pub fn find_by_phone(phone: &[u8]) -> Option<usize> {
    unsafe {
        for i in 0..RECS_N {
            if &RECS[i].phone[..RECS[i].phone_len] == phone {
                return Some(i);
            }
        }
        None
    }
}

// ==================== 搜索 ====================

/// 大小写不敏感前缀匹配（姓名拼音首字母 / 全拼 / 电话号码共用一个入口）。
pub fn prefix_match(hay: &[u8], q: &[u8]) -> bool {
    if q.is_empty() {
        return true;
    }
    if hay.len() < q.len() {
        return false;
    }
    hay[..q.len()].eq_ignore_ascii_case(q)
}

/// 遍历过滤：姓名 / 电话前缀命中 → 记录下标写入 out。返回命中数（O(n)）。
pub fn search(q: &[u8], out: &mut [usize]) -> usize {
    unsafe {
        let mut k = 0usize;
        for i in 0..RECS_N {
            if k >= out.len() {
                break;
            }
            let r = &RECS[i];
            if prefix_match(&r.name[..r.name_len], q) || prefix_match(&r.phone[..r.phone_len], q) {
                out[k] = i;
                k += 1;
            }
        }
        k
    }
}

// ==================== vCard 导入 ====================

/// 简化 vCard 解析：状态机提取 BEGIN/END:VCARD、FN、TEL、EMAIL。
/// 返回 (识别的 vCard 数, 成功入库数)。
pub fn vcf_import(data: &[u8]) -> (usize, usize) {
    let mut parsed = 0usize;
    let mut added = 0usize;
    let mut in_card = false;
    let mut name = [0u8; NAME_CAP];
    let mut name_len = 0usize;
    let mut phone = [0u8; PHONE_CAP];
    let mut phone_len = 0usize;
    let mut email = [0u8; EMAIL_CAP];
    let mut email_len = 0usize;
    let mut last: u8 = 0; // 0=无 1=FN 2=TEL 3=EMAIL（续行归属）
    let mut at = 0usize;
    while at < data.len() {
        let eol = match data[at..].iter().position(|&b| b == b'\n') {
            Some(x) => at + x,
            None => data.len(),
        };
        let line = &data[at..eol];
        at = eol + 1;
        let cont = line.len() > 0 && (line[0] == b' ' || line[0] == b'\t');
        let t = trim_ascii(line);
        if t.is_empty() {
            continue;
        }
        if cont && in_card {
            // 折叠续行：追加归属上一属性值
            match last {
                1 => name_len = append_unescape(t, &mut name, name_len),
                2 => phone_len = append_sanitize(t, &mut phone, phone_len),
                3 => email_len = append_sanitize(t, &mut email, email_len),
                _ => {}
            }
            continue;
        }
        if starts_with_ci(t, b"BEGIN:VCARD") {
            in_card = true;
            name_len = 0;
            phone_len = 0;
            email_len = 0;
            last = 0;
        } else if starts_with_ci(t, b"END:VCARD") {
            if in_card {
                parsed += 1;
                if name_len > 0 || phone_len > 0 || email_len > 0 {
                    let id = add(&name[..name_len], &phone[..phone_len], &email[..email_len], &[]);
                    if id != 0 {
                        added += 1;
                    }
                }
            }
            in_card = false;
            last = 0;
        } else if in_card {
            let (pname, v) = split_prop(t);
            if let Some(val) = v {
                let val_t = trim_ascii(val);
                if eq_ignore_case(pname, b"FN") && name_len == 0 {
                    name_len = unescape(val_t, &mut name);
                    last = 1;
                } else if eq_ignore_case(pname, b"TEL") && phone_len == 0 {
                    phone_len = sanitize(val_t, &mut phone);
                    last = 2;
                } else if eq_ignore_case(pname, b"EMAIL") && email_len == 0 {
                    email_len = sanitize(val_t, &mut email);
                    last = 3;
                }
            }
        }
    }
    (parsed, added)
}

fn append_unescape(src: &[u8], dst: &mut [u8], at: usize) -> usize {
    let mut tmp = [0u8; (NAME_CAP + ADDR_CAP) * 2 + 16];
    let n = unescape(src, &mut tmp);
    let free = dst.len().saturating_sub(at);
    let m = n.min(free);
    dst[at..at + m].copy_from_slice(&tmp[..m]);
    at + m
}

fn append_sanitize(src: &[u8], dst: &mut [u8], at: usize) -> usize {
    let mut tmp = [0u8; PHONE_CAP + EMAIL_CAP + 8];
    let n = sanitize(src, &mut tmp);
    let free = dst.len().saturating_sub(at);
    let m = n.min(free);
    dst[at..at + m].copy_from_slice(&tmp[..m]);
    at + m
}

/// 从 `/data/contacts/import.vcf` 导入；返回 (vCard 数, 入库数)。
pub fn vcf_import_file() -> (usize, usize) {
    unsafe {
        let n = read_file(IMPORT_VCARD_PATH, &mut FILE_BUF);
        if n <= 0 {
            return (0, 0);
        }
        vcf_import(&FILE_BUF[..n as usize])
    }
}

// ==================== vCard 导出 ====================

fn prop(out: &mut [u8], name: &[u8], value: &[u8], at: usize) -> usize {
    let mut a = at;
    a = push_slice(out, a, name);
    a = push_slice(out, a, b":");
    a = push_slice(out, a, value);
    a = push_slice(out, a, b"\r\n");
    a
}

/// 生成全部联系人的 vCard 3.0 文本；返回字节数（不足则截断）。
pub fn vcf_export(out: &mut [u8]) -> usize {
    unsafe {
        let mut n = 0usize;
        for i in 0..RECS_N {
            let r = RECS[i];
            n = push_slice(out, n, b"BEGIN:VCARD\r\n");
            n = push_slice(out, n, b"VERSION:3.0\r\n");
            n = push_slice(out, n, b"PRODID:-//KunxOS//KunxOS contacts//CN\r\n");
            if r.name_len > 0 {
                let mut esc = [0u8; NAME_CAP * 2 + 8];
                let e = escape(&r.name[..r.name_len], &mut esc);
                n = prop(out, b"FN", &esc[..e], n);
            }
            if r.phone_len > 0 {
                n = prop(out, b"TEL;TYPE=CELL", &r.phone[..r.phone_len], n);
            }
            if r.email_len > 0 {
                n = prop(out, b"EMAIL;TYPE=INTERNET", &r.email[..r.email_len], n);
            }
            if r.addr_len > 0 {
                let mut esc = [0u8; ADDR_CAP * 2 + 8];
                let e = escape(&r.addr[..r.addr_len], &mut esc);
                n = prop(out, b"ADR;TYPE=HOME", &esc[..e], n);
            }
            n = push_slice(out, n, b"END:VCARD\r\n");
        }
        n
    }
}

/// 导出到 `/data/contacts/export.vcf`；返回写入字节数。
pub fn vcf_export_file() -> usize {
    unsafe {
        let n = vcf_export(&mut FILE_BUF);
        if write_file(EXPORT_VCARD_PATH, &FILE_BUF[..n]) {
            n
        } else {
            0
        }
    }
}