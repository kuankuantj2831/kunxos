// KunxOS 通讯录应用（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由 launcher 经 fork + execve 拉起（/bin/contacts）。职责：
//   1. 本地数据库：/data/contacts/contacts.local（一行一个联系人：
//      id\x1F姓名\x1F电话\x1F邮箱\x1F地址，CRUD/搜索见 src/local.rs）；
//      /data/contacts.dat 仍作共享数据源（name\x1Fnum 行）供电话/短信读取
//   2. 同步存储：/data/sync/contacts.idx/.blob（vCard 格式，同步栈
//      shared/sync，见 docs/sync.md）。启动时把本地联系人导入同步存储
//      （本地记录，待推送），并把同步结果合并回列表与 contacts.dat
//   3. 联系人列表：搜索框（姓名/电话前缀）+ 头像（首字母圆）+ 姓名 + 号码；
//      点开详情可 呼叫 / 编辑 / 删除
//   4. 呼叫：写 /var/run/rild.cmd 的 DIAL 命令 → rild 拨号
//   5. 新建 / 编辑联系人：姓名 / 电话 / 邮箱 / 地址 表单（键盘输入）
//   6. 同步：配置 CardDAV 账户（服务器 / 用户名 / 密码）→ 发现地址簿 →
//      推送本地 + 拉取远端（RFC 6352 / RFC 6578，冲突手动解决）
//   7. vCard 导入/导出：/data/contacts/import.vcf ↔ export.vcf
//   8. 点击左上角「返回」或按 ESC 逐级退出，由 launcher wait4 回收

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::global_asm;
use core::panic::PanicInfo;

#[path = "../../../shared/cjk_font_contacts.rs"]
mod cjk_font;

#[path = "../../../shared/app_kit.rs"]
mod kit;

#[path = "../../../shared/sync/mod.rs"]
mod sync;

#[path = "../../../shared/sync/app_platform.rs"]
mod app_platform;

/// 本地联系人存储 + vCard 导入/导出（同目录 src/local.rs）。
mod local;

use app_platform::{SysFs, SysNet};
use kit::*;
use sync::accounts::MetaStore;
use sync::model::*;
use sync::store::Store;
use sync::sync::SyncEnv;

// ==================== 常量 ====================

const SYS_READ: usize = 63;
const SYS_WRITE: usize = 64;
const SYS_CLOSE: usize = 57;
const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const O_CREAT: usize = 0x40;

const CONTACTS_PATH: &str = "/data/contacts.dat";
const CMD_PATH: &str = "/var/run/rild.cmd";

// ==================== 颜色（浅色主题） ====================

/// 页面基底背景（浅色渐变：顶部白 → 底部极浅灰）。
const C_BG: u32 = 0x00_F7_F8_FA;
/// 主文字（白底/浅玻璃上）。
const C_NAME: u32 = 0x00_1A_1C_22;
/// 次要文字（号码）。
const C_NUM: u32 = 0x00_6B_72_80;
const C_CALL: u32 = 0x00_00_B9_6B;
/// 弱化文字/提示。
const C_DIM2: u32 = 0x00_6B_72_80;
const C_DOT: u32 = 0x00_4F_A8_FF;
const C_WARN: u32 = 0x00_F9_73_16;
const C_ACCENT: u32 = 0x00_00_B9_6B;
/// 彩色表面（头像色盘/强调绿玻璃）上的文字保持白色。
const C_ON_ACCENT: u32 = 0x00_FF_FF_FF;

const LIST_TOP: i32 = 56;
const ROW_H: i32 = 76;
const MAX_CONTACTS: usize = 1024;

/// 设备密钥（占位级：KeyStore 预留，见 docs/sync.md「安全」）。
const DEV_KEY: u32 = 0x4B58_5359;

// ==================== 联系人 ====================

#[derive(Clone, Copy)]
struct Contact {
    id: u32,     // 本地存储 id（src=0）或同步存储记录下标（src=1）
    src: u8,     // 0=本地 / 1=同步
    name: [u8; 48],
    name_len: usize,
    phone: [u8; 32],
    phone_len: usize,
    email: [u8; 64],
    email_len: usize,
    addr: [u8; 96],
    addr_len: usize,
    color: u32,
    synced: bool, // 来自同步存储
}

static mut CONTACTS: [Contact; MAX_CONTACTS] = [Contact {
    id: 0,
    src: 0,
    name: [0; 48],
    name_len: 0,
    phone: [0; 32],
    phone_len: 0,
    email: [0; 64],
    email_len: 0,
    addr: [0; 96],
    addr_len: 0,
    color: 0,
    synced: false,
}; MAX_CONTACTS];
static mut CONTACTS_N: usize = 0;

/// 内置默认联系人（首次运行写入共享存储）。
const DEFAULT_DATA: &str = "Alice\x1F13800000001\nBob\x1F13800000002\nCarol\x1F13800000003\nDave\x1F13800000004\nEve\x1F13800000005\nFrank\x1F13800000006\nGrace\x1F13800000007\nHenry\x1F13800000008\n";

fn write_dat(data: &[u8]) {
    unsafe {
        let wfd = kit::open(CONTACTS_PATH, O_WRONLY | O_CREAT);
        if wfd >= 0 {
            syscall3(SYS_WRITE, wfd as usize, data.as_ptr() as usize, data.len());
            syscall1(SYS_CLOSE, wfd as usize);
        }
    }
}

const AVATAR_COLORS: [u32; 8] = [
    0x00_E7_4C_3C,
    0x00_2E_86_C1,
    0x00_9B_59_B6,
    0x00_00_B9_6B,
    0x00_F9_73_16,
    0x00_27_AE_60,
    0x00_AF_7A_C5,
    0x00_E6_7E_22,
];

fn read_dat(buf: &mut [u8]) -> usize {
    unsafe {
        let fd = kit::open(CONTACTS_PATH, O_RDONLY);
        if fd < 0 {
            return 0;
        }
        let n = syscall3(SYS_READ, fd as usize, buf.as_mut_ptr() as usize, buf.len());
        syscall1(SYS_CLOSE, fd as usize);
        // 读失败 -1（usize 天文数字）按 isize 判负并钳到缓冲长度
        let n = n as isize;
        if n <= 0 { 0 } else { (n as usize).min(buf.len()) }
    }
}

/// 初始化本地存储：建目录并载入 contacts.local；
/// 首次运行（或旧版本升级）把 /data/contacts.dat 迁入本地数据库。
fn init_local_store() {
    unsafe {
        local::ensure_dir();
        if local::load() != 0 {
            return;
        }
        let mut buf = [0u8; 4096];
        let mut n = read_dat(&mut buf);
        if n == 0 {
            write_dat(DEFAULT_DATA.as_bytes());
            n = read_dat(&mut buf);
        }
        let mut start = 0usize;
        while start < n {
            let end = match buf[start..].iter().position(|&b| b == b'\n') {
                Some(x) => start + x,
                None => n,
            };
            let line = &buf[start..end];
            if let Some(sep) = line.iter().position(|&b| b == 0x1F) {
                local::add(&line[..sep], &line[sep + 1..], &[], &[]);
            }
            start = end + 1;
        }
        if local::count() > 0 {
            local::save();
            print("[contacts] 已将 contacts.dat 迁移到 contacts.local\n");
        }
    }
}

/// 从本地存储 + 同步存储重建渲染列表，并刷新共享 contacts.dat。
fn rebuild_list() {
    unsafe {
        CONTACTS_N = 0;
        // 本地联系人（src=0）
        for i in 0..local::count() {
            if CONTACTS_N >= MAX_CONTACTS {
                break;
            }
            let r = match local::get(i) {
                Some(r) => r,
                None => break,
            };
            let c = &mut CONTACTS[CONTACTS_N];
            c.id = r.id;
            c.src = 0;
            c.name_len = r.name_len.min(c.name.len());
            c.name[..c.name_len].copy_from_slice(&r.name[..c.name_len]);
            c.phone_len = r.phone_len.min(c.phone.len());
            c.phone[..c.phone_len].copy_from_slice(&r.phone[..c.phone_len]);
            c.email_len = r.email_len.min(c.email.len());
            c.email[..c.email_len].copy_from_slice(&r.email[..c.email_len]);
            c.addr_len = r.addr_len.min(c.addr.len());
            c.addr[..c.addr_len].copy_from_slice(&r.addr[..c.addr_len]);
            c.color = AVATAR_COLORS[CONTACTS_N % AVATAR_COLORS.len()];
            c.synced = false;
            CONTACTS_N += 1;
        }
        // 同步联系人（src=1）
        let s = store();
        let mut recs = [0usize; MAX_RECORDS];
        let n = s.query_coll(2, &mut recs);
        for k in 0..n {
            if CONTACTS_N >= MAX_CONTACTS {
                break;
            }
            let mut buf = [0u8; 8192];
            if let Some(b) = s.blob_of(recs[k]) {
                let bl = sync::util::copy_into(&mut buf, b);
                let mut vc = sync::model::Contact::new();
                if sync::vcard::parse_vcard(&mut buf, bl, &mut vc) {
                    let c = &mut CONTACTS[CONTACTS_N];
                    c.id = recs[k] as u32;
                    c.src = 1;
                    c.name_len = vc.fn_len.min(c.name.len());
                    c.name[..c.name_len].copy_from_slice(&vc.fn_[..c.name_len]);
                    let pl = if vc.tels_len > 0 { vc.tels[0].value_len } else { 0 };
                    c.phone_len = pl.min(c.phone.len());
                    c.phone[..c.phone_len].copy_from_slice(&vc.tels[0].value[..c.phone_len]);
                    let el = if vc.emails_len > 0 { vc.emails[0].value_len } else { 0 };
                    c.email_len = el.min(c.email.len());
                    c.email[..c.email_len].copy_from_slice(&vc.emails[0].value[..c.email_len]);
                    // vCard 中无“地址”字段（ORG 是公司），同步联系人暂不展示地址
                    c.addr_len = 0;
                    c.color = AVATAR_COLORS[CONTACTS_N % AVATAR_COLORS.len()];
                    c.synced = true;
                    CONTACTS_N += 1;
                }
            }
        }
        flush_dat();
    }
}

/// 把当前列表重写回共享存储（供电话/短信应用使用）。
fn flush_dat() {
    // 单行最坏 23+1+19+1 = 44 字节；1024 条 ≈ 45KB，用 64KB 缓冲一次写齐。
    let mut buf = [0u8; 65536];
    let mut n = 0usize;
    unsafe {
        for i in 0..CONTACTS_N {
            let c = &CONTACTS[i];
            let nn = c.name_len.min(23);
            let pn = c.phone_len.min(19);
            if n + nn + 1 + pn + 1 > buf.len() {
                break;
            }
            buf[n..n + nn].copy_from_slice(&c.name[..nn]);
            n += nn;
            buf[n] = 0x1F;
            n += 1;
            buf[n..n + pn].copy_from_slice(&c.phone[..pn]);
            n += pn;
            buf[n] = b'\n';
            n += 1;
        }
    }
    write_dat(&buf[..n]);
}

// ==================== 同步存储 ====================

static mut C_BLOB: [u8; 262144] = [0u8; 262144];
static mut RESP_BUF: [u8; 65536] = [0u8; 65536];
static mut REQ_BUF: [u8; 8192] = [0u8; 8192];
static mut XML_BUF: [u8; 65536] = [0u8; 65536];
static mut F_NAME: [u8; 48] = [0u8; 48];
static mut F_NAME_LEN: usize = 0;
static mut F_PHONE: [u8; 32] = [0u8; 32];
static mut F_PHONE_LEN: usize = 0;
static mut F_EMAIL: [u8; 64] = [0u8; 64];
static mut F_EMAIL_LEN: usize = 0;
static mut F_ADDR: [u8; 96] = [0u8; 96];
static mut F_ADDR_LEN: usize = 0;
static mut F_FOCUS: u8 = 0;
static mut F_SERVER: [u8; 160] = [0u8; 160];
static mut F_SERVER_LEN: usize = 0;
static mut F_USER: [u8; 64] = [0u8; 64];
static mut F_USER_LEN: usize = 0;
static mut F_PASS: [u8; 64] = [0u8; 64];
static mut F_PASS_LEN: usize = 0;
static mut SYNC_MSG: [u8; 96] = [0u8; 96];
static mut SYNC_MSG_LEN: usize = 0;
static mut SYNC_BUSY: bool = false;
/// 编辑中的本地联系人 id；u32::MAX = 非编辑（新建）。
static mut EDIT_ID: u32 = u32::MAX;
/// 列表页搜索串（姓名 / 电话前缀，纯 ASCII 过滤）。
static mut SEARCH: [u8; 32] = [0u8; 32];
static mut SEARCH_LEN: usize = 0;
/// 详情页「删除」二次确认：已武装的联系人下标。
static mut DEL_ARM: usize = usize::MAX;

static mut SYS_FS: SysFs = SysFs;
static mut SYS_NET: SysNet = SysNet;

/// 同步存储共享 blob（256KB，避免在栈上分配——用户栈 1MB，大栈帧叠加会溢出）。
static mut STORE_BLOB: [u8; 262144] = [0u8; 262144];
/// Store 静态槽位（Store 本体 128KB：recs[512]×256B；也移出栈）。
static mut STORE_SLOT: core::mem::MaybeUninit<Store<'static>> = core::mem::MaybeUninit::uninit();

/// 打开同步存储视图（进程单线程，各调用点顺序使用同一静态槽位）。
fn store() -> &'static mut Store<'static> {
    unsafe {
        let blob: &'static mut [u8] = &mut *core::ptr::addr_of_mut!(STORE_BLOB);
        let s = Store::new(blob, b"/data/sync/contacts.idx", b"/data/sync/contacts.blob");
        let slot = &mut *core::ptr::addr_of_mut!(STORE_SLOT);
        slot.write(s);
        slot.assume_init_mut()
    }
}

fn meta() -> MetaStore {
    let mut m = MetaStore::new(b"/data/sync/meta.db");
    unsafe { m.load(&mut SYS_FS); }
    m
}

/// 通过本地 id 生成同步 UID / HREF（编辑重写入站时可覆盖同一条记录）。
fn contact_uid(id: u32, out: &mut [u8]) -> usize {
    let n = sync::util::copy_into(out, b"kxc-");
    sync::util::push_dec(out, n, id as u64)
}

/// 把一条本地联系人写入同步存储（vCard，同 UID 覆盖置脏，待推送）。
fn sync_write_contact(id: u32, name: &[u8], phone: &[u8], email: &[u8]) -> bool {
    unsafe {
        let s = store();
        let mut cc = sync::model::Contact::new();
        cc.fn_len = sync::util::copy_into(&mut cc.fn_, name);
        if !phone.is_empty() {
            cc.tels[0].value_len = sync::util::copy_into(&mut cc.tels[0].value, phone);
            cc.tels[0].kind = TEL_CELL;
            cc.tels_len = 1;
        }
        if !email.is_empty() {
            cc.emails[0].value_len = sync::util::copy_into(&mut cc.emails[0].value, email);
            cc.emails[0].kind = EMAIL_HOME;
            cc.emails_len = 1;
        }
        // 集合路径（缺省 "/"）
        let mut pbuf = [0u8; 192];
        let mut pl = sync::util::copy_into(&mut pbuf, b"/");
        if let Some(c) = meta().collection(2) {
            pl = sync::util::copy_into(&mut pbuf, &c.path[..c.path_len]);
        }
        // 记录键 = href（与 UID 一一对应）
        let mut uid = [0u8; 24];
        let ul = contact_uid(id, &mut uid);
        cc.uid_len = ul;
        cc.uid[..ul].copy_from_slice(&uid[..ul]);
        let mut href = [0u8; KEY_LEN];
        let hl = sync::sync::make_href(&pbuf[..pl], &uid[..ul], b".vcf", &mut href);
        // vCard 文本 + 置脏 upsert
        let mut vcard_buf = [0u8; 8192];
        let vl = sync::vcard::vcard_to_text(&cc, &mut vcard_buf);
        let mut rec = sync::store::Rec::new();
        rec.key_len = sync::util::copy_into(&mut rec.key, &href[..hl]);
        rec.coll = 2;
        rec.mtime = now_us();
        rec.seq = 1;
        let ok = s.upsert(rec, &vcard_buf[..vl], true);
        if ok {
            let _ = s.save(&mut SYS_FS);
        }
        ok
    }
}

/// 删除同步存储中的本地联系人记录（置墓碑，下次同步向服务器发 DELETE）。
fn sync_delete_contact(id: u32) {
    unsafe {
        let mut uid = [0u8; 24];
        let ul = contact_uid(id, &mut uid);
        let mut pbuf = [0u8; 192];
        let mut pl = sync::util::copy_into(&mut pbuf, b"/");
        if let Some(c) = meta().collection(2) {
            pl = sync::util::copy_into(&mut pbuf, &c.path[..c.path_len]);
        }
        let mut href = [0u8; KEY_LEN];
        let hl = sync::sync::make_href(&pbuf[..pl], &uid[..ul], b".vcf", &mut href);
        let s = store();
        if let Some(i) = s.find(&href[..hl]) {
            s.recs[i].flags |= FLAG_DIRTY | FLAG_DELETED;
            let _ = s.save(&mut SYS_FS);
        }
    }
}

/// 启动合并：本地存储 → 同步存储（首次 vCard 导入，待推送）；
/// 同步存储 → 列表。
fn merge_stores() {
    unsafe {
        let s = store();
        let mut recs = [0usize; MAX_RECORDS];
        let first_run = s.query_coll(2, &mut recs) == 0;
        let _ = s;
        if first_run {
            let n = local::count();
            for i in 0..n {
                if let Some(r) = local::get(i) {
                    sync_write_contact(
                        r.id,
                        &r.name[..r.name_len],
                        &r.phone[..r.phone_len],
                        &r.email[..r.email_len],
                    );
                }
            }
            print("[contacts] 已将本地联系人导入同步存储\n");
        }
        rebuild_list();
        SYNC_MSG_LEN = sync::util::copy_into(&mut SYNC_MSG, "本地存储就绪".as_bytes());
    }
}

// ==================== 状态 ====================

/// u32::MAX = 列表视图；否则为详情联系人下标。
static mut VIEWING: u32 = u32::MAX;
/// 页面：0 列表 / 1 详情 / 2 新建 / 3 同步。
static mut PAGE: u8 = 0;
static mut TOAST: [u8; 40] = [0; 40];
static mut TOAST_LEN: usize = 0;
static mut TOAST_UNTIL: u64 = 0;

// ==================== 渲染 ====================

fn draw_avatar(cx: i32, cy: i32, r: i32, color: u32, ch: u8) {
    draw_disc(cx, cy, r, color);
    // 白底上用浅灰描边勾出头像轮廓。
    draw_circle(cx, cy, r, 0x00_D9_DE_E6);
    // 彩色头像盘上的首字母保持白色。
    draw_char(cx - 4, cy - 4, ch, C_ON_ACCENT);
}

const SEARCH_Y: i32 = 156;
const LIST_TOP2: i32 = 208;

fn render_list() {
    draw_gradient_bg(C_BG, glass_dark());
    draw_title_bar("通讯录".as_bytes());
    draw_glass_button(16, 110, 84, 36, "新建".as_bytes(), true);
    draw_glass_button(104, 110, 84, 36, "同步".as_bytes(), false);
    // 搜索框
    draw_glass(16, SEARCH_Y, 508, 44, 12);
    unsafe {
        if SEARCH_LEN > 0 {
            draw_text(28, SEARCH_Y + 16, &SEARCH[..SEARCH_LEN], C_TEXT);
        } else {
            draw_text(28, SEARCH_Y + 16, "搜索姓名/电话前缀".as_bytes(), C_DIM2);
        }
        draw_glass(490, SEARCH_Y + 8, 28, 28, 8);
        draw_text(497, SEARCH_Y + 16, "×".as_bytes(), C_DIM2);
    }
    unsafe {
        let mut matches = [0usize; MAX_CONTACTS];
        let mut mn = 0usize;
        for i in 0..CONTACTS_N {
            if mn >= MAX_CONTACTS {
                break;
            }
            let c = &CONTACTS[i];
            if SEARCH_LEN > 0
                && !local::prefix_match(&c.name[..c.name_len], &SEARCH[..SEARCH_LEN])
                && !local::prefix_match(&c.phone[..c.phone_len], &SEARCH[..SEARCH_LEN])
            {
                continue;
            }
            matches[mn] = i;
            mn += 1;
        }
        for mi in 0..mn {
            let c = &CONTACTS[matches[mi]];
            let y = LIST_TOP2 + mi as i32 * ROW_H;
            if y + ROW_H > DISPLAY_H as i32 {
                break;
            }
            let rh = ROW_H - 8;
            draw_glass(12, y, DISPLAY_W as i32 - 24, rh, (rh * 25 / 100).min(16));
            let initial = if c.name_len > 0 { c.name[0] } else { b'?' };
            draw_avatar(44, y + ROW_H / 2 - 4, 18, c.color, initial);
            draw_text(76, y + 14, &c.name[..c.name_len], C_NAME);
            draw_text(76, y + 40, &c.phone[..c.phone_len], C_NUM);
            if c.synced {
                draw_disc(DISPLAY_W as i32 - 34, y + ROW_H / 2 - 4, 5, C_DOT);
            }
        }
    }
    if unsafe { SYNC_MSG_LEN > 0 } {
        unsafe {
            draw_text(16, DISPLAY_H as i32 - 60, &SYNC_MSG[..SYNC_MSG_LEN], C_DIM2);
        }
    }
    draw_sys_nav();
}

/// 第 row 行（过滤后）对应的 CONTACTS 下标。
fn list_match(row: usize) -> Option<usize> {
    unsafe {
        let mut mi = 0usize;
        for i in 0..CONTACTS_N {
            if mi >= MAX_CONTACTS {
                break;
            }
            let c = &CONTACTS[i];
            if SEARCH_LEN > 0
                && !local::prefix_match(&c.name[..c.name_len], &SEARCH[..SEARCH_LEN])
                && !local::prefix_match(&c.phone[..c.phone_len], &SEARCH[..SEARCH_LEN])
            {
                continue;
            }
            if mi == row {
                return Some(i);
            }
            mi += 1;
        }
        None
    }
}

fn render_detail(idx: usize) {
    draw_gradient_bg(C_BG, glass_dark());
    let c = unsafe { &CONTACTS[idx] };

    draw_glass(0, 0, DISPLAY_W as i32, TITLE_H, 0);
    let bs = BACK_SZ - 8;
    draw_glass(BACK_X, BACK_Y, bs, bs, (bs * 25 / 100).min(16));
    draw_text(BACK_X + 10, BACK_Y + 10, b"<", C_TEXT);
    draw_text(48, (TITLE_H - 8) / 2, &c.name[..c.name_len], C_TEXT);

    let initial = if c.name_len > 0 { c.name[0] } else { b'?' };
    draw_avatar((DISPLAY_W / 2) as i32, 190, 60, c.color, initial);
    draw_center(280, &c.name[..c.name_len], C_TEXT);

    draw_text(60, 340, "手机".as_bytes(), C_DIM2);
    draw_text(120, 340, &c.phone[..c.phone_len], C_TEXT);
    let mut yl = 380;
    if c.email_len > 0 {
        draw_text(60, yl, "邮箱".as_bytes(), C_DIM2);
        draw_text(120, yl, &c.email[..c.email_len], C_TEXT);
        yl += 40;
    }
    if c.addr_len > 0 {
        draw_text(60, yl, "地址".as_bytes(), C_DIM2);
        draw_text(120, yl, &c.addr[..c.addr_len], C_TEXT);
    }

    // 操作行：删除（二次确认）/ 编辑 / 呼叫
    draw_glass_button(24, 464, 140, 60, "删除".as_bytes(), false);
    draw_glass_button(176, 464, 140, 60, "编辑".as_bytes(), false);
    unsafe {
        if DEL_ARM == idx {
            draw_glass_button(328, 464, 180, 60, "确认删除".as_bytes(), true);
        } else {
            draw_glass_style(328, 464, 180, 60, 24, &STYLE_ACCENT);
            let tx = 328 + (180 - text_width("呼叫".as_bytes())) / 2;
            // 强调绿玻璃上保持白字。
            draw_text(tx, 490, "呼叫".as_bytes(), C_ON_ACCENT);
        }
    }

    draw_text(16, DISPLAY_H as i32 - 60, "呼叫后可在电话应用查看通话".as_bytes(), C_DIM2);
    draw_sys_nav();
}

fn field(x: i32, y: i32, w: i32, label: &[u8], value: &[u8], focus: bool) {
    draw_text(x, y, label, C_DIM2);
    draw_glass(x, y + 26, w, 40, 10);
    if focus {
        draw_rect(x - 1, y + 25, w + 2, 42, C_ACCENT);
    }
    draw_text(x + 10, y + 34, value, C_TEXT);
}

fn render_new() {
    draw_gradient_bg(C_BG, glass_dark());
    unsafe {
        let editing = EDIT_ID != u32::MAX;
        draw_title_bar(if editing {
            "编辑联系人".as_bytes()
        } else {
            "新建联系人".as_bytes()
        });
        let f = F_FOCUS;
        field(24, 120, 480, "姓名".as_bytes(), &F_NAME[..F_NAME_LEN], f == 0);
        field(24, 210, 480, "电话".as_bytes(), &F_PHONE[..F_PHONE_LEN], f == 1);
        field(24, 300, 480, "邮箱".as_bytes(), &F_EMAIL[..F_EMAIL_LEN], f == 2);
        field(24, 390, 480, "地址".as_bytes(), &F_ADDR[..F_ADDR_LEN], f == 3);
    }
    draw_glass_button(24, 520, 200, 48, "保存".as_bytes(), true);
    draw_glass_button(320, 520, 200, 48, "取消".as_bytes(), false);
    draw_sys_nav();
}

fn render_sync() {
    draw_gradient_bg(C_BG, glass_dark());
    draw_title_bar("同步设置".as_bytes());
    unsafe {
        let f = F_FOCUS;
        field(24, 110, 480, "服务器 (CardDAV 根路径)".as_bytes(), &F_SERVER[..F_SERVER_LEN], f == 0);
        field(24, 200, 480, "用户名".as_bytes(), &F_USER[..F_USER_LEN], f == 1);
        field(24, 290, 480, "密码".as_bytes(), &F_PASS[..F_PASS_LEN], f == 2);
    }
    draw_glass_button(24, 380, 150, 44, "保存账户".as_bytes(), false);
    draw_glass_button(190, 380, 150, 44, "立即同步".as_bytes(), true);
    draw_glass_button(356, 380, 160, 44, "返回".as_bytes(), false);
    draw_glass_button(24, 452, 150, 44, "导入 .vcf".as_bytes(), false);
    draw_glass_button(190, 452, 150, 44, "导出 .vcf".as_bytes(), false);
    if unsafe { SYNC_MSG_LEN > 0 } {
        draw_text(24, 500, unsafe { &SYNC_MSG[..SYNC_MSG_LEN] }, C_WARN);
    }
    draw_text(24, 550, "导入/导出文件：/data/contacts/import.vcf ↔ export.vcf".as_bytes(), C_DIM2);
    draw_sys_nav();
}

fn render() {
    match unsafe { PAGE } {
        0 => render_list(),
        1 => render_detail(unsafe { VIEWING } as usize),
        2 => render_new(),
        _ => render_sync(),
    }
    // 提示气泡（toast）
    if unsafe { TOAST_LEN > 0 && now_us() < TOAST_UNTIL } {
        let t = unsafe { &TOAST[..TOAST_LEN] };
        draw_glass((DISPLAY_W as i32 - 280) / 2, 760, 280, 48, 14);
        draw_text((DISPLAY_W as i32 - 280) / 2 + 20, 782, t, C_TEXT);
    }
}

// ==================== 呼叫 ====================

fn dial(idx: usize) {
    unsafe {
        let c = &CONTACTS[idx];
        let mut cmd = [0u8; 40];
        cmd[..5].copy_from_slice(b"DIAL:");
        let nl = c.phone_len.min(30);
        cmd[5..5 + nl].copy_from_slice(&c.phone[..nl]);
        cmd[5 + nl] = b'\n';
        let fd = kit::open(CMD_PATH, O_WRONLY | O_CREAT);
        if fd >= 0 {
            syscall3(SYS_WRITE, fd as usize, cmd.as_ptr() as usize, 6 + nl);
            syscall1(SYS_CLOSE, fd as usize);
        }
        let mut msg = [0u8; 40];
        msg[..7].copy_from_slice("已拨 ".as_bytes());
        let nl2 = c.name_len.min(30);
        if 7 + nl2 <= msg.len() {
            msg[7..7 + nl2].copy_from_slice(&c.name[..nl2]);
        }
        TOAST_LEN = (7 + nl2).min(TOAST.len());
        TOAST[..TOAST_LEN].copy_from_slice(&msg[..TOAST_LEN]);
        TOAST_UNTIL = now_us() + 2_000_000;
        print("[contacts] 拨号已发送\n");
    }
}

// ==================== 业务 ====================

fn btn_hit(x: i32, y: i32, bx: i32, by: i32, bw: i32, bh: i32) -> bool {
    x >= bx && x < bx + bw && y >= by && y < by + bh
}

fn set_toast(msg: &[u8]) {
    unsafe {
        TOAST_LEN = sync::util::copy_into(&mut TOAST, msg);
        TOAST_UNTIL = now_us() + 2_000_000;
    }
}

/// 保存表单（新建或编辑）。本地联系人写入 local 库，
/// 同步存储按同 id 的 UID 覆盖置脏（待推送）。
fn save_new_contact() {
    unsafe {
        let name = &F_NAME[..F_NAME_LEN];
        let phone = &F_PHONE[..F_PHONE_LEN];
        let email = &F_EMAIL[..F_EMAIL_LEN];
        let addr = &F_ADDR[..F_ADDR_LEN];
        if name.is_empty() {
            SYNC_MSG_LEN = sync::util::copy_into(&mut SYNC_MSG, "姓名不能为空".as_bytes());
            return;
        }
        let editing = EDIT_ID != u32::MAX;
        let id = if editing {
            let id = EDIT_ID;
            if !local::update(id, name, phone, email, addr) {
                SYNC_MSG_LEN = sync::util::copy_into(&mut SYNC_MSG, "保存失败".as_bytes());
                return;
            }
            id
        } else {
            let id = local::add(name, phone, email, addr);
            if id == 0 {
                SYNC_MSG_LEN = sync::util::copy_into(&mut SYNC_MSG, "本地存储已满".as_bytes());
                return;
            }
            id
        };
        let _ = local::save();
        let ok = sync_write_contact(id, name, phone, email);
        SYNC_MSG_LEN = sync::util::copy_into(
            &mut SYNC_MSG,
            if ok {
                if editing { "已更新，待同步".as_bytes() } else { "已保存，待同步".as_bytes() }
            } else {
                "同步存储写入失败".as_bytes()
            },
        );
        rebuild_list();
        F_NAME_LEN = 0;
        F_PHONE_LEN = 0;
        F_EMAIL_LEN = 0;
        F_ADDR_LEN = 0;
        F_FOCUS = 0;
        EDIT_ID = u32::MAX;
        DEL_ARM = usize::MAX;
    }
}

/// 进入编辑表单（仅本地联系人；同步联系人暂不可编辑）。
fn edit_contact_at(idx: usize) {
    unsafe {
        let c = &CONTACTS[idx];
        if c.src != 0 {
            set_toast("同步联系人暂不可编辑".as_bytes());
            return;
        }
        F_NAME_LEN = sync::util::copy_into(&mut F_NAME, &c.name[..c.name_len]);
        F_PHONE_LEN = sync::util::copy_into(&mut F_PHONE, &c.phone[..c.phone_len]);
        F_EMAIL_LEN = sync::util::copy_into(&mut F_EMAIL, &c.email[..c.email_len]);
        F_ADDR_LEN = sync::util::copy_into(&mut F_ADDR, &c.addr[..c.addr_len]);
        F_FOCUS = 0;
        EDIT_ID = c.id;
        DEL_ARM = usize::MAX;
        PAGE = 2;
    }
}

/// 删除联系人：本地→local 库 + 同步墓碑；同步→存储墓碑（待发 DELETE）。
fn delete_contact_at(idx: usize) {
    unsafe {
        let c = &CONTACTS[idx];
        if c.src == 0 {
            local::delete(c.id);
            let _ = local::save();
            sync_delete_contact(c.id);
        } else {
            let s = store();
            if (c.id as usize) < s.n {
                s.recs[c.id as usize].flags |= FLAG_DIRTY | FLAG_DELETED;
                let _ = s.save(&mut SYS_FS);
            }
        }
        rebuild_list();
        VIEWING = u32::MAX;
        PAGE = 0;
        DEL_ARM = usize::MAX;
        set_toast("已删除".as_bytes());
    }
}

/// 从 /data/contacts/import.vcf 导入，结果提示 + 重建列表。
fn do_vcf_import() {
    unsafe {
        let (parsed, added) = local::vcf_import_file();
        if parsed == 0 {
            set_toast("未发现可导入的 vCard".as_bytes());
            return;
        }
        if added > 0 {
            let _ = local::save();
            rebuild_list();
        }
        let mut msg = [0u8; 56];
        let mut n = sync::util::copy_into(&mut msg, "导入 ".as_bytes());
        n = sync::util::push_dec(&mut msg, n, added as u64);
        n = sync::util::push_str(&mut msg, n, " 位联系人".as_bytes());
        set_toast(&msg[..n]);
    }
}

/// 导出到 /data/contacts/export.vcf。
fn do_vcf_export() {
    unsafe {
        let n = local::vcf_export_file();
        let mut msg = [0u8; 56];
        let mut m = sync::util::copy_into(&mut msg, "已导出 ".as_bytes());
        m = sync::util::push_dec(&mut msg, m, n as u64);
        m = sync::util::push_str(&mut msg, m, " 字节".as_bytes());
        set_toast(&msg[..m]);
    }
}

fn load_account_into_form() {
    unsafe {
        let m = meta();
        if let Some(a) = m.account(2) {
            F_SERVER_LEN = sync::util::copy_into(&mut F_SERVER, &a.server[..a.server_len]);
            F_USER_LEN = sync::util::copy_into(&mut F_USER, &a.username[..a.username_len]);
        }
        F_PASS_LEN = 0;
    }
}

fn save_account() {
    unsafe {
        let mut m = meta();
        let mut a = Account::new();
        a.id = 2;
        a.name_len = sync::util::copy_into(&mut a.name, b"CardDAV");
        a.acct_type = ACCT_GENERIC;
        a.server_len = sync::util::copy_into(&mut a.server, &F_SERVER[..F_SERVER_LEN]);
        a.username_len = sync::util::copy_into(&mut a.username, &F_USER[..F_USER_LEN]);
        a.auth = AUTH_BASIC;
        a.enabled = true;
        m.upsert_account(a);
        if F_PASS_LEN > 0 {
            let mut sn = [0u8; 16];
            let snl = MetaStore::account_secret_name(2, &mut sn);
            m.secret_set(&sn[..snl], &F_PASS[..F_PASS_LEN], DEV_KEY);
        }
        m.save(&mut SYS_FS);
        SYNC_MSG_LEN = sync::util::copy_into(&mut SYNC_MSG, "账户已保存".as_bytes());
    }
}

fn ensure_collection(dav: &mut sync::dav::DavClient<'_>, m: &mut MetaStore) -> bool {
    if m.collection(2).is_some() {
        return true;
    }
    unsafe {
        let mut colls = [Collection::new(); 8];
        let n = match sync::carddav::discover_addressbooks(dav, b"/", &mut colls, &mut RESP_BUF, &mut REQ_BUF, &mut XML_BUF) {
            Ok(n) => n,
            Err(_) => 0,
        };
        if n == 0 {
            SYNC_MSG_LEN = sync::util::copy_into(&mut SYNC_MSG, "未发现地址簿".as_bytes());
            return false;
        }
        let mut c = colls[0];
        c.id = 2;
        c.acct = 2;
        c.kind = COLL_ADDRBOOK;
        m.upsert_collection(c);
        m.save(&mut SYS_FS);
        SYNC_MSG_LEN = sync::util::copy_into(&mut SYNC_MSG, "已发现地址簿".as_bytes());
        true
    }
}

fn do_sync() {
    unsafe {
        SYNC_BUSY = true;
        let mut m = meta();
        let acct_ok = m.account(2).map(|a| a.enabled && a.server_len > 0).unwrap_or(false);
        if !acct_ok {
            SYNC_MSG_LEN = sync::util::copy_into(&mut SYNC_MSG, "请先配置并保存账户".as_bytes());
            SYNC_BUSY = false;
            return;
        }
        let mut env = SyncEnv {
            fs: &mut SYS_FS,
            net: &mut SYS_NET,
            dev_key: DEV_KEY,
            now_us: now_us(),
            resolve: RESOLVE_MANUAL,
        };
        let mut auth = [0u8; 260];
        let auth_len = {
            let a = m.account(2).unwrap();
            let mut pw = [0u8; SECRET_LEN];
            let mut sn = [0u8; 16];
            let snl = MetaStore::account_secret_name(2, &mut sn);
            let pl = m.secret_get(&sn[..snl], DEV_KEY, &mut pw);
            sync::http::basic_auth(&a.username[..a.username_len], &pw[..pl], &mut auth)
        };
        let mut srv = [0u8; SERVER_LEN];
        let sl = {
            let a = m.account(2).unwrap();
            sync::util::copy_into(&mut srv, &a.server[..a.server_len])
        };
        let mut dav = sync::dav::DavClient::new(&mut SYS_NET, &srv[..sl], &auth[..auth_len]);
        if !ensure_collection(&mut dav, &mut m) {
            SYNC_BUSY = false;
            return;
        }
        let mut s = store();
        let mut stats = sync::model::SyncStats::new();
        let r = sync::sync::sync_collection(&mut env, &mut m, 2, &mut s, COLL_ADDRBOOK, &mut RESP_BUF, &mut REQ_BUF, &mut XML_BUF, &mut stats);
        match r {
            Ok(()) => {
                let mut msg = [0u8; 96];
                let mut n = sync::util::copy_into(&mut msg, "同步完成 推".as_bytes());
                n = sync::util::push_dec(&mut msg, n, stats.pushed as u64);
                n = sync::util::push_str(&mut msg, n, " 拉".as_bytes());
                n = sync::util::push_dec(&mut msg, n, stats.pulled as u64);
                n = sync::util::push_str(&mut msg, n, " 冲突".as_bytes());
                n = sync::util::push_dec(&mut msg, n, stats.conflicts as u64);
                SYNC_MSG_LEN = sync::util::copy_into(&mut SYNC_MSG, &msg[..n]);
                // 同步后刷新列表 + 共享存储
                let _ = s;
                rebuild_list();
            }
            Err(e) => {
                let mut msg = [0u8; 96];
                let mut n = sync::util::copy_into(&mut msg, "同步失败: ".as_bytes());
                n = sync::util::push_str(&mut msg, n, e.label());
                SYNC_MSG_LEN = sync::util::copy_into(&mut SYNC_MSG, &msg[..n]);
            }
        }
        SYNC_BUSY = false;
    }
}

/// 同步后：重建列表（本地 + 同步合并）+ 刷新共享存储。
fn reload_list_from_store() {
    rebuild_list();
}

// ==================== 主循环 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[contacts] 正在启动\n");
    if !fb_init() {
        print("[contacts] /dev/fb0 mmap 失败，无帧缓冲\n");
        unsafe { exit(1); }
    }
    init_local_store();
    merge_stores();
    print("[contacts] 就绪，n=");
    print_dec(unsafe { CONTACTS_N } as u64);
    print("\n");

    let mut dirty = true;
    loop {
        let ev = poll_input();
        if ev.moved {
            cursor_overlay_move();
        }
        if ev.nav != NavAction::None {
            print("[nav] 系统导航 -> 退出\n");
            unsafe { exit(nav_exit_code(ev.nav)); }
        }

        if let Some((x, y)) = ev.tap {
            let page = unsafe { PAGE };
            match page {
                0 => {
                    if back_hit(x, y) {
                        print("[contacts] 返回 -> 退出\n");
                        break;
                    }
                    if btn_hit(x, y, 16, 110, 84, 36) {
                        unsafe {
                            PAGE = 2;
                            F_FOCUS = 0;
                            F_NAME_LEN = 0;
                            F_PHONE_LEN = 0;
                            F_EMAIL_LEN = 0;
                            F_ADDR_LEN = 0;
                            EDIT_ID = u32::MAX;
                        }
                    } else if btn_hit(x, y, 104, 110, 84, 36) {
                        unsafe {
                            load_account_into_form();
                            PAGE = 3;
                            F_FOCUS = 0;
                        }
                    } else if x >= 490 && x < 518 && y >= SEARCH_Y + 8 && y < SEARCH_Y + 36 {
                        // 搜索框右侧 × 清空
                        unsafe { SEARCH_LEN = 0; }
                    } else {
                        let row = (y - LIST_TOP2) / ROW_H;
                        if row >= 0 {
                            if let Some(i) = list_match(row as usize) {
                                unsafe {
                                    VIEWING = i as u32;
                                    PAGE = 1;
                                    DEL_ARM = usize::MAX;
                                }
                                print("[contacts] 打开详情\n");
                            }
                        }
                    }
                }
                1 => {
                    if back_hit(x, y) {
                        unsafe { PAGE = 0; DEL_ARM = usize::MAX; }
                    } else if btn_hit(x, y, 24, 464, 140, 60) {
                        unsafe {
                            let idx = VIEWING as usize;
                            if DEL_ARM == idx {
                                delete_contact_at(idx);
                            } else {
                                DEL_ARM = idx;
                                set_toast("再点一次确认删除".as_bytes());
                            }
                        }
                    } else if btn_hit(x, y, 176, 464, 140, 60) {
                        unsafe {
                            DEL_ARM = usize::MAX;
                            edit_contact_at(VIEWING as usize);
                        }
                    } else if btn_hit(x, y, 328, 464, 180, 60) {
                        dial(unsafe { VIEWING } as usize);
                    }
                }
                2 => {
                    if btn_hit(x, y, 24, 520, 200, 48) {
                        save_new_contact();
                        unsafe { PAGE = 0; }
                    } else if btn_hit(x, y, 320, 520, 200, 48) || back_hit(x, y) {
                        unsafe { PAGE = 0; DEL_ARM = usize::MAX; }
                    } else if y >= 120 && y < 480 {
                        unsafe { F_FOCUS = (((y - 120) / 90) as u8).min(3); }
                    }
                }
                _ => {
                    if btn_hit(x, y, 24, 380, 150, 44) {
                        save_account();
                    } else if btn_hit(x, y, 190, 380, 150, 44) {
                        save_account();
                        do_sync();
                    } else if btn_hit(x, y, 356, 380, 160, 44) || back_hit(x, y) {
                        unsafe { PAGE = 0; }
                    } else if btn_hit(x, y, 24, 452, 150, 44) {
                        do_vcf_import();
                    } else if btn_hit(x, y, 190, 452, 150, 44) {
                        do_vcf_export();
                    } else if y >= 110 && y < 260 {
                        unsafe { F_FOCUS = ((y - 110) / 90) as u8; }
                    }
                }
            }
            dirty = true;
        }
        // 键盘输入
        match ev.key {
KeyAction::Char(c) => {
        let page = unsafe { PAGE };
        if page == 0 {
            // 列表页：字符直接进搜索框（姓名/电话前缀过滤）
            unsafe {
                if SEARCH_LEN < SEARCH.len() {
                    SEARCH[SEARCH_LEN] = c;
                    SEARCH_LEN += 1;
                }
            }
            dirty = true;
        } else if page == 2 || page == 3 {
            let focus = unsafe { F_FOCUS };
            unsafe {
                let (buf, len): (&mut [u8], &mut usize) = match (page, focus) {
                    (2, 0) => (&mut F_NAME, &mut F_NAME_LEN),
                    (2, 1) => (&mut F_PHONE, &mut F_PHONE_LEN),
                    (2, 2) => (&mut F_EMAIL, &mut F_EMAIL_LEN),
                    (2, 3) => (&mut F_ADDR, &mut F_ADDR_LEN),
                    (3, 0) => (&mut F_SERVER, &mut F_SERVER_LEN),
                    (3, 1) => (&mut F_USER, &mut F_USER_LEN),
                    (3, 2) => (&mut F_PASS, &mut F_PASS_LEN),
                    _ => continue,
                };
                if *len < buf.len() {
                    buf[*len] = c;
                    *len += 1;
                }
            }
            dirty = true;
        }
    }
    KeyAction::Backspace => {
        let page = unsafe { PAGE };
        if page == 0 {
            unsafe {
                if SEARCH_LEN > 0 {
                    SEARCH_LEN -= 1;
                }
            }
            dirty = true;
        } else if page == 2 || page == 3 {
            let focus = unsafe { F_FOCUS };
            unsafe {
                let len: &mut usize = match (page, focus) {
                    (2, 0) => &mut F_NAME_LEN,
                    (2, 1) => &mut F_PHONE_LEN,
                    (2, 2) => &mut F_EMAIL_LEN,
                    (2, 3) => &mut F_ADDR_LEN,
                    (3, 0) => &mut F_SERVER_LEN,
                    (3, 1) => &mut F_USER_LEN,
                    (3, 2) => &mut F_PASS_LEN,
                    _ => continue,
                };
                if *len > 0 {
                    *len -= 1;
                }
            }
            dirty = true;
        }
    }
    KeyAction::Enter => {
        let page = unsafe { PAGE };
        if page == 0 {
            // 列表页回车 = 清空搜索
            unsafe { SEARCH_LEN = 0; }
            dirty = true;
        } else if page == 2 {
            unsafe {
                if F_FOCUS < 3 {
                    F_FOCUS += 1;
                } else {
                    save_new_contact();
                    PAGE = 0;
                }
            }
            dirty = true;
        } else if page == 3 {
            unsafe {
                if F_FOCUS < 2 {
                    F_FOCUS += 1;
                } else {
                    save_account();
                    do_sync();
                }
            }
            dirty = true;
        }
    }
    KeyAction::Esc => {
        let page = unsafe { PAGE };
        if page != 0 {
            unsafe {
                PAGE = 0;
                DEL_ARM = usize::MAX;
            }
            dirty = true;
        } else if unsafe { VIEWING } != u32::MAX {
            unsafe {
                VIEWING = u32::MAX;
                DEL_ARM = usize::MAX;
            }
            dirty = true;
        } else {
            print("[contacts] ESC -> 退出\n");
            break;
        }
    }
            _ => {}
        }
        if poll_tty_esc() {
            print("[contacts] ESC -> 退出\n");
            break;
        }
        if dirty {
            render();
            flip();
            cursor_overlay_redraw();
            dirty = false;
            kit::font_init_checked();
            if kit::font_just_refreshed() { dirty = true; }
        }
        sleep_ms(16);
    }

    print("[contacts] 正在退出\n");
    unsafe { exit(0); }
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
    // 诊断：把 panic 消息与位置（文件:行:列）写入 fd 2（无堆，固定缓冲）。
    struct BufWriter {
        buf: [u8; 256],
        len: usize,
    }
    impl core::fmt::Write for BufWriter {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let b = s.as_bytes();
            let k = b.len().min(self.buf.len() - self.len);
            self.buf[self.len..self.len + k].copy_from_slice(&b[..k]);
            self.len += k;
            Ok(())
        }
    }
    let mut w = BufWriter { buf: [0u8; 256], len: 0 };
    let _ = core::fmt::Write::write_fmt(&mut w, core::format_args!("{}\n", info));
    unsafe {
        write_fd(2, b"PANIC: contacts\n");
        write_fd(2, &w.buf[..w.len]);
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
