//! 配置存储：/data/settings/<分类>.conf（手写 key=value）+ 即时发布到子系统消费端。
//!
//! `cfg::set` 为公开写入入口：写内存缓冲 -> 落盘 -> 写 /data/settings/notify 变更
//! 标记 -> `apply` 把变化镜像/下发给既有消费者（/data/oobe/*、/var/run/wifid.pipe、
//! 电源 syscall 411、云账户文件等），实现实时生效。

use super::*;

pub(crate) const CONF_NET: &str = "network.conf";
pub(crate) const CONF_DISPLAY: &str = "display.conf";
pub(crate) const CONF_SOUND: &str = "sound.conf";
pub(crate) const CONF_NOTIF: &str = "notifications.conf";
pub(crate) const CONF_SEC: &str = "security.conf";
pub(crate) const CONF_ACCT: &str = "accounts.conf";
pub(crate) const CONF_SYSTEM: &str = "system.conf";
pub(crate) const CONF_BATTERY: &str = "battery.conf";
pub(crate) const CONF_STORAGE: &str = "storage.conf";
pub(crate) const CONF_ACCESS: &str = "accessibility.conf";

pub(crate) const SETTINGS_DIR: &str = "/data/settings";
pub(crate) const NOTIFY_PATH: &str = "/data/settings/notify";

pub(crate) const MODULES_PATH: &str = "/data/oobe/modules";
pub(crate) const NAV_MODE_PATH: &str = "/data/oobe/nav_mode";
pub(crate) const THEME_PATH: &str = "/data/oobe/theme";
pub(crate) const BRIGHTNESS_PATH: &str = "/data/oobe/brightness";
pub(crate) const VOLUME_PATH: &str = "/data/oobe/volume";
pub(crate) const LANG_PATH: &str = "/data/oobe/lang";
pub(crate) const TZ_PATH: &str = "/data/oobe/timezone";
pub(crate) const SEC_TYPE_PATH: &str = "/data/oobe/security_type";
pub(crate) const SEC_HASH_PATH: &str = "/data/oobe/security_hash";
pub(crate) const DURESS_HASH_PATH: &str = "/data/oobe/duress_hash";
pub(crate) const DURESS_ACTION_PATH: &str = "/data/oobe/duress_action";
pub(crate) const LOCAL_ACCOUNT_PATH: &str = "/data/oobe/local_account";
pub(crate) const CLOUD_ENABLED_PATH: &str = "/data/oobe/cloud_enabled";
pub(crate) const CLOUD_ACCOUNT_PATH: &str = "/data/oobe/cloud_account";
pub(crate) const CLOUD_SERVER_PATH: &str = "/data/oobe/cloud_server";
pub(crate) const WIFID_PIPE: &str = "/var/run/wifid.pipe";
pub(crate) const WIFI_NET_DIR: &str = "/data/wifi/networks";
pub(crate) const PERM_DIR: &str = "/var/lib/pkg/permissions";
pub(crate) const OTA_DIR: &str = "/data/ota";

static mut BUF_NET: [u8; 512] = [0u8; 512];
static mut BUF_DISPLAY: [u8; 512] = [0u8; 512];
static mut BUF_SOUND: [u8; 512] = [0u8; 512];
static mut BUF_NOTIF: [u8; 768] = [0u8; 768];
static mut BUF_SEC: [u8; 640] = [0u8; 640];
static mut BUF_ACCT: [u8; 512] = [0u8; 512];
static mut BUF_SYSTEM: [u8; 512] = [0u8; 512];
static mut BUF_BATTERY: [u8; 256] = [0u8; 256];
static mut BUF_STORAGE: [u8; 256] = [0u8; 256];
static mut BUF_ACCESS: [u8; 256] = [0u8; 256];

static mut LEN_NET: usize = 0;
static mut LEN_DISPLAY: usize = 0;
static mut LEN_SOUND: usize = 0;
static mut LEN_NOTIF: usize = 0;
static mut LEN_SEC: usize = 0;
static mut LEN_ACCT: usize = 0;
static mut LEN_SYSTEM: usize = 0;
static mut LEN_BATTERY: usize = 0;
static mut LEN_STORAGE: usize = 0;
static mut LEN_ACCESS: usize = 0;

/// 在 key=value 文件中查找 key，返回值切片（不含换行）。未找到返回 None。
fn kv_get<'a>(buf: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let mut i = 0;
    while i < buf.len() {
        let e = buf[i..]
            .iter()
            .position(|&c| c == b'\n')
            .map(|p| i + p)
            .unwrap_or(buf.len());
        let line = &buf[i..e];
        if line.len() > key.len() && line[key.len()] == b'=' && starts_with(line, key) {
            return Some(trim_space(&line[key.len() + 1..]));
        }
        i = e + 1;
    }
    None
}

pub(crate) fn cfg_get_str(cat: &str, key: &[u8], def: &[u8], out: &mut [u8]) -> usize {
    let buf = cfg_buf(cat);
    match kv_get(buf, key) {
        Some(v) => {
            let n = v.len().min(out.len());
            out[..n].copy_from_slice(&v[..n]);
            n
        }
        None => {
            let n = def.len().min(out.len());
            out[..n].copy_from_slice(&def[..n]);
            n
        }
    }
}

pub(crate) fn cfg_get_int(cat: &str, key: &[u8], def: i32) -> i32 {
    let buf = cfg_buf(cat);
    kv_get(buf, key).and_then(parse_int).unwrap_or(def)
}

/// 更新（或追加）key=value 到内存缓冲（不落盘）。
unsafe fn cfg_buf_set(cat: &str, key: &[u8], val: &[u8]) -> bool {
    let buf: *mut u8 = cfg_buf_mut(cat);
    let len: *mut usize = cfg_len_mut(cat);
    let cap = cfg_cap(cat);
    let data = core::slice::from_raw_parts_mut(buf, cap);
    let n = *len;
    let mut found = false;
    let mut i = 0;
    while i < n && !found {
        let e = data[i..n]
            .iter()
            .position(|&c| c == b'\n')
            .map(|p| i + p)
            .unwrap_or(n);
        if e - i > key.len() && starts_with(&data[i..e], key) && data[i + key.len()] == b'=' {
            // 重写本行值。
            let newlen = key.len() + 1 + val.len() + 1;
            let tail = n.saturating_sub(e + 1);
            if i + newlen + tail > cap {
                return false;
            }
            // ★ 搬移缓冲必须覆盖最大 cap（BUF_NOTIF=768）：tmp 只有 512 时，
            // notifications.conf 前 512B 内的 key 被重写即 copy_from_slice
            // 长度不匹配 panic。
            let mut tmp = [0u8; 768];
            if tail > tmp.len() {
                return false;
            }
            if tail > 0 {
                tmp[..tail].copy_from_slice(&data[e + 1..n]);
            }
            data[i..i + key.len()].copy_from_slice(key);
            data[i + key.len()] = b'=';
            data[i + key.len() + 1..i + newlen - 1].copy_from_slice(val);
            data[i + newlen - 1] = b'\n';
            if tail > 0 {
                data[i + newlen..i + newlen + tail].copy_from_slice(&tmp[..tail]);
            }
            *len = i + newlen + tail;
            found = true;
        }
        i = e + 1;
    }
    if !found {
        let need = key.len() + 1 + val.len() + 1;
        let tail = if n > 0 && data[n - 1] != b'\n' {
            1
        } else {
            0
        };
        if n + tail + need > cap {
            return false;
        }
        if tail > 0 {
            data[n] = b'\n';
        }
        let base = n + tail;
        data[base..base + key.len()].copy_from_slice(key);
        data[base + key.len()] = b'=';
        data[base + key.len() + 1..base + need - 1].copy_from_slice(val);
        data[base + need - 1] = b'\n';
        *len = base + need;
    }
    true
}

/// 公开写入入口：缓冲 + 落盘 + 变更标记 + 子系统发布。
pub(crate) unsafe fn cfg_set(cat: &str, key: &[u8], val: &[u8]) -> bool {
    if !cfg_buf_set(cat, key, val) {
        return false;
    }
    // 落盘。
    let ok = cfg_save(cat);
    // 变更标记。
    cfg_notify(cat, key);
    // 子系统发布。
    apply(cat, key);
    ok
}

/// 整数写入入口。
pub(crate) unsafe fn cfg_set_int(cat: &str, key: &[u8], v: i32) -> bool {
    let mut num = [0u8; 12];
    let n = fmt_dec(&mut num, v as u32);
    cfg_set(cat, key, &num[..n])
}

pub(crate) fn cfg_save(cat: &str) -> bool {
    unsafe {
        let buf = cfg_buf(cat);
        let len = cfg_len(cat);
        let mut path = [0u8; 80];
        path[..SETTINGS_DIR.len()].copy_from_slice(SETTINGS_DIR.as_bytes());
        path[SETTINGS_DIR.len()] = b'/';
        let cs = SETTINGS_DIR.len() + 1;
        path[cs..cs + cat.len()].copy_from_slice(cat.as_bytes());
        let path_str = core::str::from_utf8(&path[..cs + cat.len()]).unwrap_or("/data/settings/x.conf");
        write_file(path_str, &buf[..len])
    }
}

/// 把变化镜像/下发给既有消费者（实时生效路径）。
unsafe fn apply(cat: &str, key: &[u8]) {
    unsafe { apply_impl(cat, key) }
}

fn apply_impl(cat: &str, key: &[u8]) {
    // 亮度 -> /data/oobe/brightness
    if cat == CONF_DISPLAY && key == b"brightness" {
        let v = cfg_get_int(CONF_DISPLAY, b"brightness", 85).clamp(0, 100);
        let mut num = [0u8; 12];
        let n = fmt_dec(&mut num, v as u32);
        unsafe { write_file(BRIGHTNESS_PATH, &num[..n]); }
    }
    // 深色 -> /data/oobe/theme（保留 wallpaper 后缀）
    else if cat == CONF_DISPLAY && key == b"dark_mode" {
        mirror_theme();
    }
    // 媒体音量 -> /data/oobe/volume（launcher 主音量）
    else if cat == CONF_SOUND && key == b"vol_media" {
        let v = cfg_get_int(CONF_SOUND, b"vol_media", 60).clamp(0, 100);
        let mut num = [0u8; 12];
        let n = fmt_dec(&mut num, v as u32);
        unsafe { write_file(VOLUME_PATH, &num[..n]); }
    }
    // 语言 -> /data/oobe/lang
    else if cat == CONF_SYSTEM && key == b"lang" {
        let mut v = [0u8; 16];
        let n = cfg_get_str(CONF_SYSTEM, b"lang", b"zh_CN", &mut v);
        unsafe { write_file(LANG_PATH, &v[..n]); }
    }
    // 时区 -> /data/oobe/timezone
    else if cat == CONF_SYSTEM && key == b"timezone" {
        let mut v = [0u8; 32];
        let n = cfg_get_str(CONF_SYSTEM, b"timezone", b"Asia/Shanghai", &mut v);
        unsafe { write_file(TZ_PATH, &v[..n]); }
    }
    // Wi-Fi 开关 -> wifid.pipe
    else if cat == CONF_NET && key == b"wifi_enabled" {
        if cfg_get_int(CONF_NET, b"wifi_enabled", 0) != 0 {
            unsafe { wifi_cmd(b"AUTO"); }
        } else {
            unsafe { wifi_cmd(b"DISCONNECT"); }
        }
    }
    // 锁屏类型/哈希 -> /data/oobe/security_*
    else if cat == CONF_SEC && key == b"lock_type" {
        let mut v = [0u8; 16];
        let n = cfg_get_str(CONF_SEC, b"lock_type", b"slide", &mut v);
        unsafe { write_file(SEC_TYPE_PATH, &v[..n]); }
    }
    else if cat == CONF_SEC && key == b"lock_hash" {
        let mut v = [0u8; 16];
        let n = cfg_get_str(CONF_SEC, b"lock_hash", b"", &mut v);
        unsafe { write_file(SEC_HASH_PATH, &v[..n]); }
        // 同步本地账户口令哈希部分（local_account = 用户名:哈希）
        sync_local_account_hash(&v[..n]);
    }
    else if cat == CONF_SEC && key == b"duress_hash" {
        let mut v = [0u8; 16];
        let n = cfg_get_str(CONF_SEC, b"duress_hash", b"", &mut v);
        unsafe { write_file(DURESS_HASH_PATH, &v[..n]); }
    }
    else if cat == CONF_SEC && key == b"duress_action" {
        let mut v = [0u8; 16];
        let n = cfg_get_str(CONF_SEC, b"duress_action", b"fake_system", &mut v);
        unsafe { write_file(DURESS_ACTION_PATH, &v[..n]); }
    }
    // 本地账户用户名 -> /data/oobe/local_account（保留哈希部分）
    else if cat == CONF_ACCT && key == b"local_user" {
        sync_local_account_user();
    }
    // 云开关/账户
    else if cat == CONF_ACCT && key == b"cloud_enabled" {
        let v = cfg_get_int(CONF_ACCT, b"cloud_enabled", 0);
        let mut num = [0u8; 4];
        let n = fmt_dec(&mut num, v as u32);
        unsafe { write_file(CLOUD_ENABLED_PATH, &num[..n]); }
    }
    else if cat == CONF_ACCT && key == b"cloud_account" {
        let mut v = [0u8; 80];
        let n = cfg_get_str(CONF_ACCT, b"cloud_account", b"", &mut v);
        unsafe { write_file(CLOUD_ACCOUNT_PATH, &v[..n]); }
    }
    // 低电量模式 -> governor syscall
    else if cat == CONF_BATTERY && key == b"low_power" {
        if cfg_get_int(CONF_BATTERY, b"low_power", 0) != 0 {
            unsafe { syscall1(SYS_PM_GOVERNOR, GOV_POWERSAVE); }
        } else {
            unsafe { syscall1(SYS_PM_GOVERNOR, GOV_ONDEMAND); }
        }
    }
    else {
        // 其余为预留项：仅落盘 + notify。
    }
}

fn cfg_notify(cat: &str, key: &[u8]) {
    unsafe {
        let mut line = [0u8; 80];
        let mut n = 0;
        for &c in cat.as_bytes() {
            line[n] = c;
            n += 1;
        }
        line[n] = b' ';
        n += 1;
        for &c in key {
            line[n] = c;
            n += 1;
        }
        line[n] = b'\n';
        n += 1;
        write_file(NOTIFY_PATH, &line[..n]);
    }
}

fn mirror_theme() {
    unsafe {
        let mut cur = [0u8; 48];
        let n = read_file(THEME_PATH, &mut cur);
        let mut wall: &[u8] = b"wallpaper:2";
        if n > 0 {
            let v = trim_space(&cur[..n as usize]);
            if let Some(pos) = v.iter().position(|&c| c == b',') {
                if pos + 1 < v.len() {
                    wall = &v[pos + 1..];
                }
            }
        }
        let mode: &[u8] = if dark_on() { b"dark" } else { b"light" };
        let mut line = [0u8; 56];
        let mut i = 0;
        for &c in mode {
            line[i] = c;
            i += 1;
        }
        line[i] = b',';
        i += 1;
        for &c in wall {
            line[i] = c;
            i += 1;
        }
        write_file(THEME_PATH, &line[..i]);
    }
}

pub(crate) fn dark_on() -> bool {
    cfg_get_int(CONF_DISPLAY, b"dark_mode", 1) != 0
}

pub(crate) fn brightness_level() -> i32 {
    cfg_get_int(CONF_DISPLAY, b"brightness", 85).clamp(0, 100)
}

pub(crate) fn invert_on() -> bool {
    cfg_get_int(CONF_ACCESS, b"invert_colors", 0) != 0
}

pub(crate) fn contrast_on() -> bool {
    cfg_get_int(CONF_ACCESS, b"high_contrast", 0) != 0
}

// ==================== 目录与消费端工具 ====================

/// 向 wifid 写一条命令（覆盖写）。
pub(crate) unsafe fn wifi_cmd(line: &[u8]) -> bool {
    let mut buf = [0u8; 128];
    let n = line.len().min(buf.len() - 1);
    buf[..n].copy_from_slice(&line[..n]);
    buf[n] = b'\n';
    write_file(WIFID_PIPE, &buf[..n + 1])
}

/// 刷新已保存网络清单（/data/wifi/networks 文件名即 SSID）。
pub(crate) unsafe fn refresh_saved_wifi() {
    let mut raw = [0u8; 2048];
    let n = list_dir(WIFI_NET_DIR, &mut raw);
    let mut cnt = 0usize;
    let mut i = 0usize;
    while i < n && cnt < SAVED_SSIDS.len() {
        let e = raw[i..n]
            .iter()
            .position(|&c| c == 0)
            .map(|p| i + p)
            .unwrap_or(n);
        let name = &raw[i..e];
        if !name.is_empty() {
            let ln = name.len().min(SAVED_SSIDS[cnt].len() - 1);
            SAVED_SSIDS[cnt][..ln].copy_from_slice(&name[..ln]);
            cnt += 1;
        }
        i = e + 1;
    }
    SAVED_COUNT = cnt;
}

/// 刷新应用清单（/data/oobe/modules CSV）。
pub(crate) unsafe fn refresh_apps() {
    let mut buf = [0u8; 512];
    let n = read_file(MODULES_PATH, &mut buf);
    let nb = if n > 0 { n as usize } else { 0 };
    let mut ci = 0usize;
    let mut i = 0usize;
    while i < nb && ci < APP_NAMES.len() {
        while i < nb && (is_sep(buf[i]) || buf[i] == b',') {
            i += 1;
        }
        if i >= nb {
            break;
        }
        let s = i;
        while i < nb && buf[i] != b',' {
            i += 1;
        }
        let name = &buf[s..i];
        if !name.is_empty() {
            let ln = name.len().min(APP_NAMES[ci].len() - 1);
            APP_NAMES[ci][..ln].copy_from_slice(&name[..ln]);
            ci += 1;
        }
    }
    APP_COUNT = ci;
}

/// 刷新 OTA 包清单（/data/ota/*.kxup）。
pub(crate) unsafe fn refresh_ota() {
    let mut raw = [0u8; 512];
    let n = list_dir(OTA_DIR, &mut raw);
    let mut cnt = 0usize;
    let mut i = 0usize;
    while i < n && cnt < OTA_PKGS.len() {
        let e = raw[i..n]
            .iter()
            .position(|&c| c == 0)
            .map(|p| i + p)
            .unwrap_or(n);
        let name = &raw[i..e];
        if !name.is_empty() && find_sub(name, b".kxup") {
            let ln = name.len().min(OTA_PKGS[cnt].len() - 1);
            OTA_PKGS[cnt][..ln].copy_from_slice(&name[..ln]);
            cnt += 1;
        }
        i = e + 1;
    }
    OTA_COUNT = cnt;
}

/// 刷新电源状态缓存（syscall 410）。
pub(crate) fn refresh_pm() -> u64 {
    unsafe { syscall1(SYS_PM_GET, 0) as u64 }
}

// ==================== 哈希（与 launcher/OOBE/cloud 一致） ====================

pub(crate) fn fnv1a(data: &[u8]) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for &b in data {
        h ^= b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    h
}

/// 8 位小写 hex（FNV-1a32，与 launcher 锁屏校验一致）。
pub(crate) fn fnv1a_hex(data: &[u8], out: &mut [u8; 8]) {
    let h = fnv1a(data);
    for i in 0..8 {
        let nib = (h >> ((7 - i) * 4)) & 0xF;
        out[i] = b"0123456789abcdef"[nib as usize];
    }
}

/// 16 位小写 hex（FNV-1a64，云令牌）。
pub(crate) fn fnv1a64_hex(data: &[u8], out: &mut [u8; 16]) {
    let mut h: u64 = 0xCBF2_9CE4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x1_0000_0001_B3);
    }
    for i in 0..16 {
        let nib = (h >> ((15 - i) * 4)) & 0xF;
        out[i] = b"0123456789abcdef"[nib as usize];
    }
}

// ==================== local_account 同步 ====================

/// local_account = "<用户名>:<哈希>"。用户改名时保留哈希。
fn sync_local_account_user() {
    unsafe {
        let mut la = [0u8; 64];
        let n = read_file(LOCAL_ACCOUNT_PATH, &mut la);
        let mut hash: &[u8] = b"";
        if n > 0 {
            let v = trim_space(&la[..n as usize]);
            if let Some(pos) = v.iter().position(|&c| c == b':') {
                if pos + 1 < v.len() {
                    hash = &v[pos + 1..];
                }
            }
        }
        let mut user = [0u8; 32];
        let un = cfg_get_str(CONF_ACCT, b"local_user", b"user", &mut user);
        let mut line = [0u8; 96];
        let mut i = 0;
        for &c in &user[..un] {
            line[i] = c;
            i += 1;
        }
        line[i] = b':';
        i += 1;
        for &c in hash {
            line[i] = c;
            i += 1;
        }
        write_file(LOCAL_ACCOUNT_PATH, &line[..i]);
    }
}

/// 修改锁屏哈希时同步到 local_account 的哈希部分。
fn sync_local_account_hash(hash: &[u8]) {
    unsafe {
        let mut user = [0u8; 32];
        let un = cfg_get_str(CONF_ACCT, b"local_user", b"user", &mut user);
        let mut line = [0u8; 96];
        let mut i = 0;
        for &c in &user[..un] {
            line[i] = c;
            i += 1;
        }
        line[i] = b':';
        i += 1;
        for &c in hash {
            line[i] = c;
            i += 1;
        }
        write_file(LOCAL_ACCOUNT_PATH, &line[..i]);
    }
}

// ==================== 初始化 ====================

fn cfg_buf(cat: &str) -> &'static [u8] {
    unsafe {
        if cat == CONF_NET {
            &*core::ptr::addr_of!(BUF_NET)
        } else if cat == CONF_DISPLAY {
            &*core::ptr::addr_of!(BUF_DISPLAY)
        } else if cat == CONF_SOUND {
            &*core::ptr::addr_of!(BUF_SOUND)
        } else if cat == CONF_NOTIF {
            &*core::ptr::addr_of!(BUF_NOTIF)
        } else if cat == CONF_SEC {
            &*core::ptr::addr_of!(BUF_SEC)
        } else if cat == CONF_ACCT {
            &*core::ptr::addr_of!(BUF_ACCT)
        } else if cat == CONF_SYSTEM {
            &*core::ptr::addr_of!(BUF_SYSTEM)
        } else if cat == CONF_BATTERY {
            &*core::ptr::addr_of!(BUF_BATTERY)
        } else if cat == CONF_STORAGE {
            &*core::ptr::addr_of!(BUF_STORAGE)
        } else {
            &*core::ptr::addr_of!(BUF_ACCESS)
        }
    }
}

fn cfg_buf_mut(cat: &str) -> *mut u8 {
    unsafe {
        if cat == CONF_NET {
            core::ptr::addr_of_mut!(BUF_NET).cast()
        } else if cat == CONF_DISPLAY {
            core::ptr::addr_of_mut!(BUF_DISPLAY).cast()
        } else if cat == CONF_SOUND {
            core::ptr::addr_of_mut!(BUF_SOUND).cast()
        } else if cat == CONF_NOTIF {
            core::ptr::addr_of_mut!(BUF_NOTIF).cast()
        } else if cat == CONF_SEC {
            core::ptr::addr_of_mut!(BUF_SEC).cast()
        } else if cat == CONF_ACCT {
            core::ptr::addr_of_mut!(BUF_ACCT).cast()
        } else if cat == CONF_SYSTEM {
            core::ptr::addr_of_mut!(BUF_SYSTEM).cast()
        } else if cat == CONF_BATTERY {
            core::ptr::addr_of_mut!(BUF_BATTERY).cast()
        } else if cat == CONF_STORAGE {
            core::ptr::addr_of_mut!(BUF_STORAGE).cast()
        } else {
            core::ptr::addr_of_mut!(BUF_ACCESS).cast()
        }
    }
}

fn cfg_len_mut(cat: &str) -> *mut usize {
    unsafe {
        if cat == CONF_NET {
            core::ptr::addr_of_mut!(LEN_NET)
        } else if cat == CONF_DISPLAY {
            core::ptr::addr_of_mut!(LEN_DISPLAY)
        } else if cat == CONF_SOUND {
            core::ptr::addr_of_mut!(LEN_SOUND)
        } else if cat == CONF_NOTIF {
            core::ptr::addr_of_mut!(LEN_NOTIF)
        } else if cat == CONF_SEC {
            core::ptr::addr_of_mut!(LEN_SEC)
        } else if cat == CONF_ACCT {
            core::ptr::addr_of_mut!(LEN_ACCT)
        } else if cat == CONF_SYSTEM {
            core::ptr::addr_of_mut!(LEN_SYSTEM)
        } else if cat == CONF_BATTERY {
            core::ptr::addr_of_mut!(LEN_BATTERY)
        } else if cat == CONF_STORAGE {
            core::ptr::addr_of_mut!(LEN_STORAGE)
        } else {
            core::ptr::addr_of_mut!(LEN_ACCESS)
        }
    }
}

fn cfg_len(cat: &str) -> usize {
    unsafe { *cfg_len_mut(cat) }
}

fn cfg_cap(cat: &str) -> usize {
    if cat == CONF_NET {
        512
    } else if cat == CONF_DISPLAY {
        512
    } else if cat == CONF_SOUND {
        512
    } else if cat == CONF_NOTIF {
        768
    } else if cat == CONF_SEC {
        640
    } else if cat == CONF_ACCT {
        512
    } else if cat == CONF_SYSTEM {
        512
    } else if cat == CONF_BATTERY {
        256
    } else if cat == CONF_STORAGE {
        256
    } else {
        256
    }
}

/// 加载一个分类文件（不存在则为空）。
pub(crate) unsafe fn cfg_load(cat: &str) {
    let buf = cfg_buf_mut(cat);
    let len = cfg_len_mut(cat);
    let cap = cfg_cap(cat);
    let mut path = [0u8; 80];
    path[..SETTINGS_DIR.len()].copy_from_slice(SETTINGS_DIR.as_bytes());
    path[SETTINGS_DIR.len()] = b'/';
    let cs = SETTINGS_DIR.len() + 1;
    path[cs..cs + cat.len()].copy_from_slice(cat.as_bytes());
    let path_str = core::str::from_utf8(&path[..cs + cat.len()]).unwrap_or("/data/settings/x.conf");
    let data = core::slice::from_raw_parts_mut(buf, cap);
    let n = read_file(path_str, data);
    *len = if n > 0 { n as usize } else { 0 };
}

/// 首页：建目录、加载、默认种子、刷新清单。
pub(crate) unsafe fn settings_init() {
    let _ = mkdir_path(SETTINGS_DIR);
    let _ = mkdir_path(OTA_DIR);
    cfg_load(CONF_NET);
    cfg_load(CONF_DISPLAY);
    cfg_load(CONF_SOUND);
    cfg_load(CONF_NOTIF);
    cfg_load(CONF_SEC);
    cfg_load(CONF_ACCT);
    cfg_load(CONF_SYSTEM);
    cfg_load(CONF_BATTERY);
    cfg_load(CONF_STORAGE);
    cfg_load(CONF_ACCESS);
    seed_all();
    refresh_apps();
    refresh_saved_wifi();
    refresh_ota();
    // 持久化的低电量模式 -> governor。
    if cfg_get_int(CONF_BATTERY, b"low_power", 0) != 0 {
        syscall1(SYS_PM_GOVERNOR, GOV_POWERSAVE);
    }
}

pub(crate) unsafe fn seed_all() {
    let seed = |cat: &str, key: &[u8], val: &[u8]| cfg_set(cat, key, val);
    let seedi = |cat: &str, key: &[u8], v: i32| {
        let mut num = [0u8; 12];
        let n = fmt_dec(&mut num, v as u32);
        seed(cat, key, &num[..n])
    };
    let _ = seed;
    let _ = seedi;

    if LEN_NET == 0 {
        let mut num = [0u8; 12];
        let n = fmt_dec(&mut num, 0);
        cfg_set(CONF_NET, b"wifi_enabled", &num[..n]);
        let n = fmt_dec(&mut num, 0);
        cfg_set(CONF_NET, b"bt_enabled", &num[..n]);
        let n = fmt_dec(&mut num, 0);
        cfg_set(CONF_NET, b"data_enabled", &num[..n]);
        let n = fmt_dec(&mut num, 0);
        cfg_set(CONF_NET, b"vpn_enabled", &num[..n]);
        let n = fmt_dec(&mut num, 0);
        cfg_set(CONF_NET, b"nfc_enabled", &num[..n]);
        cfg_set(CONF_NET, b"ip_mode", b"dhcp");
        cfg_set(CONF_NET, b"ip_addr", b"192.168.1.100");
        cfg_set(CONF_NET, b"ip_gw", b"192.168.1.1");
        cfg_set(CONF_NET, b"ip_dns", b"10.0.2.3");
    }
    if LEN_DISPLAY == 0 {
        cfg_set_int(CONF_DISPLAY, b"brightness", 85);
        cfg_set_int(CONF_DISPLAY, b"font_size", 1);
        cfg_set_int(CONF_DISPLAY, b"dark_mode", 1);
        cfg_set(CONF_DISPLAY, b"resolution", b"540x960");
        cfg_set_int(CONF_DISPLAY, b"refresh_rate", 120);
        cfg_set_int(CONF_DISPLAY, b"auto_rotate", 0);
    }
    if LEN_SOUND == 0 {
        cfg_set_int(CONF_SOUND, b"vol_media", 60);
        cfg_set_int(CONF_SOUND, b"vol_ring", 60);
        cfg_set_int(CONF_SOUND, b"vol_alarm", 60);
        cfg_set_int(CONF_SOUND, b"vol_call", 60);
        cfg_set_int(CONF_SOUND, b"vibration", 1);
        cfg_set(CONF_SOUND, b"ringtone", b"classic");
        cfg_set_int(CONF_SOUND, b"dnd_enabled", 0);
        cfg_set_int(CONF_SOUND, b"dnd_start", 22);
        cfg_set_int(CONF_SOUND, b"dnd_end", 7);
    }
    if LEN_NOTIF == 0 {
        cfg_set_int(CONF_NOTIF, b"push_enabled", 1);
        cfg_set(CONF_NOTIF, b"preview", b"full");
    }
    if LEN_SEC == 0 {
        // 从 /data/oobe 继承既有锁屏设置（如已设）。
        let mut st = [0u8; 16];
        let n = read_file(SEC_TYPE_PATH, &mut st);
        if n > 0 {
            let v = trim_space(&st[..n as usize]);
            cfg_set(CONF_SEC, b"lock_type", v);
        } else {
            cfg_set(CONF_SEC, b"lock_type", b"slide");
        }
        let mut sh = [0u8; 16];
        let n = read_file(SEC_HASH_PATH, &mut sh);
        if n > 0 {
            cfg_set(CONF_SEC, b"lock_hash", trim_space(&sh[..n as usize]));
        } else {
            cfg_set(CONF_SEC, b"lock_hash", b"");
        }
        let mut dh = [0u8; 16];
        let n = read_file(DURESS_HASH_PATH, &mut dh);
        if n > 0 {
            cfg_set(CONF_SEC, b"duress_hash", trim_space(&dh[..n as usize]));
        } else {
            cfg_set(CONF_SEC, b"duress_hash", b"");
        }
        let mut da = [0u8; 16];
        let n = read_file(DURESS_ACTION_PATH, &mut da);
        if n > 0 {
            cfg_set(CONF_SEC, b"duress_action", trim_space(&da[..n as usize]));
        } else {
            cfg_set(CONF_SEC, b"duress_action", b"fake_system");
        }
        cfg_set_int(CONF_SEC, b"location", 1);
        // 广告标识符：无 RNG，以开机时间派生 8 hex 作占位。
        let t = now_us() as u32;
        let mut tmp = [0u8; 8];
        for i in 0..8 {
            let nib = (t >> ((7 - i) * 4)) & 0xF;
            tmp[i] = b"0123456789abcdef"[nib as usize];
        }
        cfg_set(CONF_SEC, b"ad_id", &tmp[..8]);
    }
    if LEN_ACCT == 0 {
        cfg_set(CONF_ACCT, b"device_name", b"KunxPhone");
        let mut la = [0u8; 64];
        let n = read_file(LOCAL_ACCOUNT_PATH, &mut la);
        if n > 0 {
            let v = trim_space(&la[..n as usize]);
            let colon = v.iter().position(|&c| c == b':').unwrap_or(v.len());
            cfg_set(CONF_ACCT, b"local_user", &v[..colon]);
        } else {
            cfg_set(CONF_ACCT, b"local_user", b"user");
        }
        cfg_set_int(CONF_ACCT, b"cloud_enabled", 0);
        let mut ca = [0u8; 64];
        let n = read_file(CLOUD_ACCOUNT_PATH, &mut ca);
        if n > 0 {
            cfg_set(CONF_ACCT, b"cloud_account", trim_space(&ca[..n as usize]));
        }
        cfg_set_int(CONF_ACCT, b"sync_enabled", 1);
    }
    if LEN_SYSTEM == 0 {
        let mut lg = [0u8; 16];
        let n = read_file(LANG_PATH, &mut lg);
        if n > 0 {
            cfg_set(CONF_SYSTEM, b"lang", trim_space(&lg[..n as usize]));
        } else {
            cfg_set(CONF_SYSTEM, b"lang", b"zh_CN");
        }
        let mut tz = [0u8; 32];
        let n = read_file(TZ_PATH, &mut tz);
        if n > 0 {
            cfg_set(CONF_SYSTEM, b"timezone", trim_space(&tz[..n as usize]));
        } else {
            cfg_set(CONF_SYSTEM, b"timezone", b"Asia/Shanghai");
        }
        cfg_set_int(CONF_SYSTEM, b"auto_time", 1);
        cfg_set_int(CONF_SYSTEM, b"fmt_24h", 1);
        cfg_set(CONF_SYSTEM, b"keyboard", b"builtin");
        cfg_set_int(CONF_SYSTEM, b"usb_debug", 0);
        cfg_set_int(CONF_SYSTEM, b"oem_unlock", 0);
    }
    if LEN_BATTERY == 0 {
        cfg_set_int(CONF_BATTERY, b"show_percent", 1);
        cfg_set_int(CONF_BATTERY, b"low_power", 0);
    }
    if LEN_STORAGE == 0 {
        cfg_set_int(CONF_STORAGE, b"cache_cleaned", 0);
    }
    if LEN_ACCESS == 0 {
        cfg_set_int(CONF_ACCESS, b"magnifier", 0);
        cfg_set_int(CONF_ACCESS, b"invert_colors", 0);
        cfg_set_int(CONF_ACCESS, b"high_contrast", 0);
    }
}

// ==================== 通用发布 ====================
// 这些函数供 pages 直接调用（apply 已覆盖大部分；特殊动作在此补充）。

/// 校验当前输入的旧锁屏值（PIN/密码/图案）是否匹配已存哈希。
/// type 1=pin 2=password 3=pattern；返回是否匹配。
pub(crate) fn verify_old_secret(ty: u8, input: &[u8]) -> bool {
    let mut stored = [0u8; 16];
    let n = cfg_get_str(CONF_SEC, b"lock_hash", b"", &mut stored);
    if n == 0 {
        return true; // 未设置旧密码：视为通过。
    }
    let mut h = [0u8; 8];
    if ty == 3 {
        // 图案：输入为 1..9 节点序列 -> 数字串哈希。
        let mut seq = [0u8; 16];
        let mut si = 0;
        for &v in input {
            if si < 16 {
                seq[si] = b'0' + v;
                si += 1;
            }
        }
        fnv1a_hex(&seq[..si], &mut h);
    } else {
        fnv1a_hex(input, &mut h);
    }
    h == stored[..8]
}

/// 校验两个输入是否相等（hex 比较用）。
pub(crate) fn hex_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a == b
}