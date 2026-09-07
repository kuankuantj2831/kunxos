// KunxOS 天气应用（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由 launcher 经 fork + execve 拉起（/bin/weather）。职责：
//   1. HTTP 获取：直连公开天气 API（Open-Meteo，HTTP 明文，无需 API Key），
//      复用提示词 #16 的 HTTP 客户端模式（socket syscall + DNS + HTTP/1.0 +
//      Connection: close），响应读入全局静态缓冲。
//   2. JSON 解析：手写极简状态机提取器（不绑 serde），单遍扫描，只提取
//      current.* 与 daily.* 中渲染需要的字段（温度/湿度/体感/风速/天气码/5 日预报）。
//   3. 本地缓存：/data/weather/cache_<city>.dat（SFS 文件），头行时间戳 +
//      原始 JSON，30 分钟过期；网络失败时回退到过期缓存。
//   4. 数据接口：weather_load() / weather_cur() / weather_day() / weather_state()
//      供渲染层查询；成功后写入 /data/oobe/weather_temp 与 weather_cond，
//      供 launcher 桌面天气小组件展示。
//   5. 城市：内置 6 个城市坐标（先手动选城市，不做定位），点城市名或按 C 切换；
//      点「刷新」或按 R 强制刷新。
//   6. 点击左上角「返回」或按 ESC 退出（exit(0)），由 launcher wait4 回收。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::global_asm;
use core::panic::PanicInfo;

#[path = "../../../shared/cjk_font_weather.rs"]
mod cjk_font;

#[path = "../../../shared/app_kit.rs"]
mod kit;

mod json;

use kit::*;

// ==================== 布局与配色 ====================

const C_BG: u32 = 0x00_F7_F8_FA;
const C_CARD: u32 = 0x00_FF_FF_FF;
const C_CARD_BORDER: u32 = 0x00_D9_DE_E6;
const C_TEXT: u32 = 0x00_1A_1C_22;
const C_DIM: u32 = 0x00_6B_72_80;
// 天气图标色（浅色背景上需可读：过亮的雪白/云灰/闪电黄适当压暗）。
const C_SUN: u32 = 0x00_F5_A6_23;
const C_CLOUD: u32 = 0x00_9A_A8_B8;
const C_RAIN: u32 = 0x00_2E_86_C1;
const C_SNOW: u32 = 0x00_8A_B8_DC;
const C_THUNDER: u32 = 0x00_E2_96_06;
const C_ACCENT: u32 = 0x00_00_B9_6B;
const C_WARN: u32 = 0x00_E2_96_06;

// ==================== 城市表（内置坐标，预格式化十进制避免浮点格式化） ====================

struct City {
    name: &'static str,
    key: &'static str, // 缓存文件名用（ASCII）
    lat: &'static str,
    lon: &'static str,
}

static CITIES: [City; 6] = [
    City {
        name: "北京",
        key: "beijing",
        lat: "39.9042",
        lon: "116.4074",
    },
    City {
        name: "上海",
        key: "shanghai",
        lat: "31.2304",
        lon: "121.4737",
    },
    City {
        name: "广州",
        key: "guangzhou",
        lat: "23.1291",
        lon: "113.2644",
    },
    City {
        name: "深圳",
        key: "shenzhen",
        lat: "22.5431",
        lon: "114.0579",
    },
    City {
        name: "成都",
        key: "chengdu",
        lat: "30.5728",
        lon: "104.0668",
    },
    City {
        name: "杭州",
        key: "hangzhou",
        lat: "30.2741",
        lon: "120.1551",
    },
];

// ==================== 数据模型 ====================

// CurrentWeather / DayForecast 定义在 json.rs（纯模块，可在宿主单测）。
use json::{wmo_icon, wmo_label, wmo_widget, CurrentWeather, DayForecast};

static mut W_CITY: usize = 0;
static mut W_OK: bool = false; // 是否有可用数据
static mut W_STALE: bool = false; // 数据是否过期（网络失败用了旧缓存）
static mut W_TS: u64 = 0; // 数据时间戳（unix 秒）

// 无数据/加载中时的兜底展示（等价旧版静态数据）。
static FALLBACK_CUR: CurrentWeather = CurrentWeather {
    temp: 26,
    feels: 30,
    humidity: 45,
    wind: 12,
    code: 2,
    is_day: true,
};
static FALLBACK_DAYS: [DayForecast; 5] = [
    DayForecast {
        code: 0,
        high: 28,
        low: 19,
    },
    DayForecast {
        code: 2,
        high: 27,
        low: 18,
    },
    DayForecast {
        code: 61,
        high: 24,
        low: 17,
    },
    DayForecast {
        code: 2,
        high: 26,
        low: 18,
    },
    DayForecast {
        code: 0,
        high: 29,
        low: 20,
    },
];

static mut W_CUR: CurrentWeather = CurrentWeather::empty();
static mut W_DAYS: [DayForecast; 5] = [DayForecast::empty(); 5];

// ==================== 通用工具 ====================

fn starts_with(a: &[u8], p: &[u8]) -> bool {
    a.len() >= p.len() && a[..p.len()] == *p
}

fn find_sub(data: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || data.len() < needle.len() {
        return None;
    }
    let mut i = 0;
    while i + needle.len() <= data.len() {
        if &data[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// 十进制格式化（无前导零），返回长度。
fn fmt_dec(mut v: u64, buf: &mut [u8]) -> usize {
    if v == 0 {
        if buf.is_empty() {
            return 0;
        }
        buf[0] = b'0';
        return 1;
    }
    let mut pos = buf.len();
    while v > 0 {
        if pos == 0 {
            break; // buf 容不下更多数字，防下溢
        }
        pos -= 1;
        buf[pos] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    let n = buf.len() - pos;
    buf.copy_within(pos.., 0);
    n
}

/// 有符号十进制格式化，返回长度。
fn fmt_i32(v: i32, buf: &mut [u8]) -> usize {
    if v < 0 {
        buf[0] = b'-';
        1 + fmt_dec(v.unsigned_abs() as u64, &mut buf[1..])
    } else {
        fmt_dec(v as u64, buf)
    }
}

/// 读取整个小文件到 buf，返回长度；失败返回 -1。
fn read_file(path: &str, buf: &mut [u8]) -> isize {
    unsafe {
        let fd = open(path, O_RDONLY);
        if fd < 0 {
            return -1;
        }
        let mut total = 0usize;
        while total < buf.len() {
            let n = syscall3(
                SYS_READ,
                fd as usize,
                buf.as_mut_ptr().add(total) as usize,
                buf.len() - total,
            ) as isize;
            if n <= 0 {
                break;
            }
            total += n as usize;
        }
        syscall1(SYS_CLOSE, fd as usize);
        total as isize
    }
}

/// 写整个小文件（O_CREAT|O_TRUNC|O_WRONLY）。
fn write_file(path: &str, data: &[u8]) -> bool {
    unsafe {
        let fd = open(path, 0x40 | 0x200 | 1);
        if fd < 0 {
            return false;
        }
        let mut off = 0usize;
        while off < data.len() {
            let n = syscall3(
                SYS_WRITE,
                fd as usize,
                data.as_ptr().add(off) as usize,
                data.len() - off,
            ) as isize;
            if n <= 0 {
                break;
            }
            off += n as usize;
        }
        syscall1(SYS_CLOSE, fd as usize);
        off == data.len()
    }
}

const SYS_MKDIRAT: usize = 34;

static mut MKDIR_BUF: [u8; 64] = [0; 64];

/// 创建目录（幂等；已存在返回 -17 可忽略）。
fn mkdir(path: &str) -> isize {
    unsafe {
        let n = path.len().min(MKDIR_BUF.len() - 1);
        MKDIR_BUF[..n].copy_from_slice(path.as_bytes());
        MKDIR_BUF[n] = 0;
        syscall3(SYS_MKDIRAT, AT_FDCWD, MKDIR_BUF.as_ptr() as usize, 0o755) as isize
    }
}

// ==================== HTTP 客户端（复用提示词 #16 模式） ====================

/// Open-Meteo：公开天气 API，HTTP 明文（端口 80），无需 API Key。
const HOST: &[u8] = b"api.open-meteo.com";
const PORT: u16 = 80;

static mut URL: [u8; 384] = [0; 384];
static mut REQ: [u8; 640] = [0; 640];
static mut RESP: [u8; 8192] = [0; 8192];

/// 拼接当前城市的 Open-Meteo 查询 URL（只请求渲染需要的字段，响应约 1KB）。
fn build_url() -> &'static [u8] {
    unsafe {
        let c = &CITIES[W_CITY];
        let mut n = 0usize;
        for &x in b"/v1/forecast?latitude=" {
            URL[n] = x;
            n += 1;
        }
        for &x in c.lat.as_bytes() {
            URL[n] = x;
            n += 1;
        }
        for &x in b"&longitude=" {
            URL[n] = x;
            n += 1;
        }
        for &x in c.lon.as_bytes() {
            URL[n] = x;
            n += 1;
        }
        for &x in b"&current=temperature_2m,relative_humidity_2m,apparent_temperature,is_day,weather_code,wind_speed_10m&daily=weather_code,temperature_2m_max,temperature_2m_min&forecast_days=5&timezone=auto"
        {
            URL[n] = x;
            n += 1;
        }
        &URL[..n]
    }
}

/// 发送 HTTP GET（HTTP/1.0 + Connection: close），完整响应读入全局 RESP。
/// 返回响应总字节数；失败返回负值。
fn http_get(path: &[u8]) -> isize {
    unsafe {
        let mut n = 0usize;
        for &c in b"GET " {
            REQ[n] = c;
            n += 1;
        }
        for &c in path {
            REQ[n] = c;
            n += 1;
        }
        for &c in b" HTTP/1.0\r\nHost: api.open-meteo.com\r\nConnection: close\r\n\r\n" {
            REQ[n] = c;
            n += 1;
        }
        let req = &REQ[..n];

        let ip = dns_resolve(HOST);
        if ip == 0 {
            return -10; // DNS 解析失败
        }
        let fd = sock_open();
        if fd < 0 {
            return -11;
        }
        if sock_connect(fd, ip, PORT) < 0 {
            sock_close(fd);
            return -12;
        }
        let mut sent = 0usize;
        while sent < req.len() {
            let r = sock_send(fd, &req[sent..]);
            if r <= 0 {
                sock_close(fd);
                return -13;
            }
            sent += r as usize;
        }
        let mut total = 0usize;
        while total < RESP.len() {
            let r = sock_recv(fd, &mut RESP[total..]);
            if r < 0 {
                sock_close(fd);
                return -14;
            }
            if r == 0 {
                break;
            }
            total += r as usize;
        }
        sock_close(fd);
        total as isize
    }
}

/// 取响应 body（跳过 "HTTP/1.x ...\r\n\r\n"）。返回 body 切片。
fn resp_body(resp: &[u8]) -> &[u8] {
    if let Some(i) = find_sub(resp, b"\r\n\r\n") {
        let s = i + 4;
        if s <= resp.len() {
            return &resp[s..];
        }
    }
    resp
}


// ==================== 缓存管理（SFS 文件 + 时间戳过期） ====================

const CACHE_DIR: &str = "/data/weather";
const CACHE_TTL: u64 = 1800; // 30 分钟
const WIDGET_TEMP: &str = "/data/oobe/weather_temp";
const WIDGET_COND: &str = "/data/oobe/weather_cond";

static mut CACHE_PATH: [u8; 64] = [0; 64];

fn cache_path() -> &'static str {
    unsafe {
        let c = &CITIES[W_CITY];
        let mut n = 0usize;
        for &x in b"/data/weather/cache_" {
            CACHE_PATH[n] = x;
            n += 1;
        }
        for &x in c.key.as_bytes() {
            CACHE_PATH[n] = x;
            n += 1;
        }
        for &x in b".dat" {
            CACHE_PATH[n] = x;
            n += 1;
        }
        core::str::from_utf8(&CACHE_PATH[..n]).unwrap_or("")
    }
}

/// 缓存文件格式：`KXW1 <unix秒>` + `<原始 JSON body>`。
fn cache_save(ts: u64, body: &[u8]) -> bool {
    let mut hdr = [0u8; 40];
    let mut n = 0usize;
    hdr[n..n + 5].copy_from_slice(b"KXW1 ");
    n += 5;
    let mut num = [0u8; 20];
    let nl = fmt_dec(ts, &mut num);
    hdr[n..n + nl].copy_from_slice(&num[..nl]);
    n += nl;
    hdr[n] = b'\n';
    n += 1;
    unsafe {
        let fd = open(cache_path(), 0x40 | 0x200 | 1); // O_CREAT|O_TRUNC|O_WRONLY
        if fd < 0 {
            return false;
        }
        let mut ok = true;
        let mut off = 0usize;
        while off < n {
            let r = syscall3(
                SYS_WRITE,
                fd as usize,
                hdr.as_ptr().add(off) as usize,
                n - off,
            ) as isize;
            if r <= 0 {
                ok = false;
                break;
            }
            off += r as usize;
        }
        if ok {
            let mut off = 0usize;
            while off < body.len() {
                let r = syscall3(
                    SYS_WRITE,
                    fd as usize,
                    body.as_ptr().add(off) as usize,
                    body.len() - off,
                ) as isize;
                if r <= 0 {
                    ok = false;
                    break;
                }
                off += r as usize;
            }
        }
        syscall1(SYS_CLOSE, fd as usize);
        ok
    }
}

/// 读取缓存。成功返回 (true, ts, body 长度)，body 已前移到 RESP 开头。
fn cache_load() -> (bool, u64, usize) {
    unsafe {
        let n = read_file(cache_path(), &mut RESP);
        if n < 0 {
            return (false, 0, 0);
        }
        let b = &RESP[..n as usize];
        if !starts_with(b, b"KXW1 ") {
            return (false, 0, 0);
        }
        let mut ts: u64 = 0;
        let mut j = 5usize;
        let mut any = false;
        while j < b.len() && b[j] >= b'0' && b[j] <= b'9' {
            ts = ts.saturating_mul(10).saturating_add((b[j] - b'0') as u64);
            j += 1;
            any = true;
        }
        if !any || j >= b.len() || b[j] != b'\n' {
            return (false, 0, 0);
        }
        let body_len = b.len() - j - 1;
        RESP.copy_within(j + 1.., 0);
        (true, ts, body_len)
    }
}

// ==================== 数据接口（供渲染层查询） ====================

/// 确保 /data/weather 目录存在（幂等）。
fn ensure_cache_dir() {
    let r = mkdir(CACHE_DIR);
    if r != 0 && r != -17 {
        print("[weather] mkdir /data/weather 失败： ");
        print_dec((-r) as u64);
        print("\n");
    }
}

/// 把当前数据写入 launcher 桌面小组件文件。
fn widget_sync() {
    unsafe {
        let mut tb = [0u8; 8];
        let n = fmt_i32(W_CUR.temp, &mut tb);
        write_file(WIDGET_TEMP, &tb[..n]);
        write_file(WIDGET_COND, wmo_widget(W_CUR.code));
    }
}

/// 网络拉取当前城市。成功返回 true 并提交数据模型。
fn weather_fetch() -> bool {
    unsafe {
        let url = build_url();
        print("[weather] GET http://api.open-meteo.com");
        print(core::str::from_utf8(url).unwrap_or(""));
        print("\n");
        let total = http_get(url);
        if total <= 0 {
            print("[weather] http_get 失败： ");
            print_dec((-total) as u64);
            print("\n");
            return false;
        }
        print("[weather] 已获取 ");
        print_dec(total as u64);
        print(" 字节\n");
        let body = resp_body(&RESP[..total as usize]);
        // 解析到临时副本，成功后才提交（失败不污染现有数据）。
        let mut cur = CurrentWeather::empty();
        let mut days = [DayForecast::empty(); 5];
        if !json::json_parse(body, &mut cur, &mut days) {
            print("[weather] JSON 解析失败\n");
            return false;
        }
        W_CUR = cur;
        W_DAYS = days;
        let now = now_us() / 1_000_000;
        W_TS = now;
        W_STALE = false;
        W_OK = true;
        cache_save(now, body);
        widget_sync();
        error_ui::err_clear();
        print("[weather] 成功，temp=");
        print_dec(W_CUR.temp as u64);
        print("C code=");
        print_dec(W_CUR.code as u64);
        print("\n");
        true
    }
}

/// 加载当前城市：新鲜缓存 → 直接用；否则网络拉取；网络失败 → 过期缓存兜底。
/// 返回状态码：0 新数据，1 过期但可用，2 无数据。
fn weather_load() -> u8 {
    unsafe {
        ensure_cache_dir();
        let now = now_us() / 1_000_000;
        // 1. 新鲜缓存
        let (exists, ts, blen) = cache_load();
        if exists && now >= ts && now - ts < CACHE_TTL {
            let mut cur = CurrentWeather::empty();
            let mut days = [DayForecast::empty(); 5];
            if json::json_parse(&RESP[..blen], &mut cur, &mut days) {
                W_CUR = cur;
                W_DAYS = days;
                W_TS = ts;
                W_STALE = false;
                W_OK = true;
                error_ui::err_clear();
                print("[weather] 缓存命中 ts=");
                print_dec(ts);
                print("\n");
                return 0;
            }
        }
        // 2. 网络拉取
        if weather_fetch() {
            return 0;
        }
        // 3. 过期缓存兜底（重新读取，RESP 可能已被失败的拉取污染）
        let (exists, ts, blen) = cache_load();
        if exists && blen > 0 {
            let mut cur = CurrentWeather::empty();
            let mut days = [DayForecast::empty(); 5];
            if json::json_parse(&RESP[..blen], &mut cur, &mut days) {
                W_CUR = cur;
                W_DAYS = days;
                W_TS = ts;
                W_STALE = true;
                W_OK = true;
                error_ui::err_clear();
                print("[weather] 缓存过期，回退使用\n");
                return 1;
            }
        }
        W_OK = false;
        W_STALE = false;
        print("[weather] 无可用数据\n");
        2
    }
}

/// 当前数据状态：0 新，1 过期但可用，2 无数据。
fn weather_state() -> u8 {
    unsafe {
        if !W_OK {
            2
        } else if W_STALE {
            1
        } else {
            0
        }
    }
}

/// 当前城市名。
fn weather_city() -> &'static str {
    unsafe { CITIES[W_CITY].name }
}

fn weather_cur() -> &'static CurrentWeather {
    unsafe {
        if W_OK {
            &W_CUR
        } else {
            &FALLBACK_CUR
        }
    }
}

fn weather_day(i: usize) -> &'static DayForecast {
    unsafe {
        if W_OK {
            &W_DAYS[i]
        } else {
            &FALLBACK_DAYS[i]
        }
    }
}

/// 切换城市（新城市无数据时回退到旧城市旧数据）。
fn weather_cycle() {
    unsafe {
        let old = W_CITY;
        let next = (old + 1) % CITIES.len();
        let (old_ok, old_stale, old_ts) = (W_OK, W_STALE, W_TS);
        W_CITY = next;
        print("[weather] 切换到 ");
        print(CITIES[next].name);
        print("\n");
        let st = weather_load();
        if st == 2 && old != next {
            W_CITY = old;
            W_OK = old_ok;
            W_STALE = old_stale;
            W_TS = old_ts;
            print("[weather] 新城市无数据，已回退到 ");
            print(CITIES[old].name);
            print("\n");
        }
    }
}

/// 强制刷新当前城市（失败保留旧数据并显示错误横幅）。
fn weather_refresh() {
    unsafe {
        print("[weather] 刷新\n");
        let (old_ok, old_stale, old_ts) = (W_OK, W_STALE, W_TS);
        if !weather_fetch() {
            W_OK = old_ok;
            W_STALE = old_stale;
            W_TS = old_ts;
            error_ui::err_set("天气数据获取失败，请检查网络".as_bytes(), now_us());
        }
    }
}

// ==================== 渲染 ====================

/// 太阳：圆 + 光芒。
fn draw_sun(cx: i32, cy: i32, r: i32) {
    draw_disc(cx, cy, r, C_SUN);
    for k in 0..8 {
        let deg = k * 45;
        let (x1, y1) = ray_end(cx, cy, r + 6, deg);
        let (x2, y2) = ray_end(cx, cy, r + 12, deg);
        draw_line(x1, y1, x2, y2, C_SUN);
    }
}

fn ray_end(cx: i32, cy: i32, l: i32, deg: i32) -> (i32, i32) {
    // 简单 8 方向
    match deg {
        0 => (cx, cy - l),
        45 => (cx + l / 2, cy - l / 2),
        90 => (cx + l, cy),
        135 => (cx + l / 2, cy + l / 2),
        180 => (cx, cy + l),
        225 => (cx - l / 2, cy + l / 2),
        270 => (cx - l, cy),
        _ => (cx - l / 2, cy - l / 2),
    }
}

/// 云：三个圆并排。
fn draw_cloud(cx: i32, cy: i32) {
    draw_disc(cx - 16, cy + 6, 12, C_CLOUD);
    draw_disc(cx, cy, 16, C_CLOUD);
    draw_disc(cx + 18, cy + 6, 12, C_CLOUD);
}

/// 雨：云 + 斜线。
fn draw_rain(cx: i32, cy: i32) {
    draw_cloud(cx, cy);
    draw_line(cx - 12, cy + 24, cx - 18, cy + 36, C_RAIN);
    draw_line(cx + 4, cy + 24, cx - 2, cy + 36, C_RAIN);
    draw_line(cx + 20, cy + 24, cx + 14, cy + 36, C_RAIN);
}

/// 雪：云 + 小雪花点。
fn draw_snow(cx: i32, cy: i32) {
    draw_cloud(cx, cy);
    draw_disc(cx - 12, cy + 30, 3, C_SNOW);
    draw_disc(cx + 4, cy + 34, 3, C_SNOW);
    draw_disc(cx + 20, cy + 30, 3, C_SNOW);
}

/// 雷雨：云 + 闪电。
fn draw_thunder(cx: i32, cy: i32) {
    draw_cloud(cx, cy);
    draw_line(cx + 2, cy + 22, cx - 6, cy + 34, C_THUNDER);
    draw_line(cx - 6, cy + 34, cx + 2, cy + 34, C_THUNDER);
    draw_line(cx + 2, cy + 34, cx - 2, cy + 46, C_THUNDER);
    draw_line(cx - 2, cy + 46, cx + 8, cy + 34, C_THUNDER);
}

/// 雾：云 + 底部横线。
fn draw_fog(cx: i32, cy: i32) {
    draw_cloud(cx, cy);
    draw_line(cx - 16, cy + 30, cx + 20, cy + 30, C_CLOUD);
    draw_line(cx - 12, cy + 38, cx + 16, cy + 38, C_CLOUD);
}

fn cond_icon(cx: i32, cy: i32, kind: u8) {
    match kind {
        0 => draw_sun(cx, cy, 18),
        2 => draw_rain(cx, cy),
        3 => draw_snow(cx, cy),
        4 => draw_thunder(cx, cy),
        5 => draw_fog(cx, cy),
        _ => draw_cloud(cx, cy),
    }
}

/// 第 i 天标签：今天 / 明天 / 周X（本地日期，UTC+8）。
fn day_label(i: usize) -> &'static str {
    if i == 0 {
        "今天"
    } else if i == 1 {
        "明天"
    } else {
        let us = now_us();
        let days = ((us / 1_000_000) as i64 + 8 * 3600) / 86400 + i as i64;
        match weekday_from_days(days) {
            1 => "周一",
            2 => "周二",
            3 => "周三",
            4 => "周四",
            5 => "周五",
            6 => "周六",
            _ => "周日",
        }
    }
}

/// 带边界检查的单字节写入（越界时静默丢弃，n 不再前进）。
fn push_u8(buf: &mut [u8], n: &mut usize, b: u8) {
    if *n < buf.len() {
        buf[*n] = b;
        *n += 1;
    }
}

/// 把温度写入 buf（含 ° 后缀），返回长度。
fn push_temp(buf: &mut [u8; 16], mut n: usize, v: i32) -> usize {
    if v < 0 {
        if n < buf.len() {
            buf[n] = b'-';
        }
        n += 1;
    }
    let mut tmp = [0u8; 12];
    let mut i = tmp.len();
    let mut v = v.unsigned_abs();
    while v > 0 && i > 0 {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    if i == tmp.len() {
        if n < buf.len() {
            buf[n] = b'0';
        }
        n += 1;
    } else {
        while i < tmp.len() {
            if n < buf.len() {
                buf[n] = tmp[i];
            }
            n += 1;
            i += 1;
        }
    }
    if n + 1 < buf.len() {
        buf[n] = 0xC2; // °
        buf[n + 1] = 0xB0;
    }
    n + 2
}

/// "HH:MM"（本地时间，UTC+8）。
fn fmt_hhmm(ts: u64, buf: &mut [u8]) -> usize {
    let local = ts as i64 + 8 * 3600;
    let h = ((local / 3600) % 24) as u64;
    let m = ((local / 60) % 60) as u64;
    let mut n = 0;
    buf[n] = b'0' + (h / 10) as u8;
    n += 1;
    buf[n] = b'0' + (h % 10) as u8;
    n += 1;
    buf[n] = b':';
    n += 1;
    buf[n] = b'0' + (m / 10) as u8;
    n += 1;
    buf[n] = b'0' + (m % 10) as u8;
    n += 1;
    n
}

fn render() {
    draw_gradient_bg(C_BG, glass_dark());
    draw_title_bar("天气".as_bytes());

    let st = weather_state();
    let cur = weather_cur();
    let city = weather_city();

    // 当前天气卡（液态玻璃面板）
    draw_glass(16, 64, DISPLAY_W as i32 - 32, 240, 24);
    draw_text(32, 84, city.as_bytes(), C_TEXT);
    draw_text(32, 106, "点击切换".as_bytes(), C_DIM);
    // 右上角刷新
    draw_text(DISPLAY_W as i32 - 70, 84, "刷新".as_bytes(), C_ACCENT);
    // 状况 + 昼夜
    let mut cond = [0u8; 32];
    let mut cn = 0;
    let label = wmo_label(cur.code).as_bytes();
    cond[..label.len()].copy_from_slice(label);
    cn += label.len();
    cond[cn] = 0x20;
    cn += 1;
    let dn = if cur.is_day {
        "白天".as_bytes()
    } else {
        "夜间".as_bytes()
    };
    cond[cn..cn + dn.len()].copy_from_slice(dn);
    cn += dn.len();
    draw_text(32, 132, &cond[..cn], C_DIM);
    // 大温度
    let mut t = [0u8; 16];
    let n = push_temp(&mut t, 0, cur.temp);
    if n < t.len() {
        t[n] = b'C';
    }
    draw_text(200, 96, &t[..(n + 1).min(t.len())], C_TEXT);
    cond_icon(360, 170, wmo_icon(cur.code));
    // 附加信息
    let mut info = [0u8; 48];
    let mut in_ = 0;
    for &x in "湿度 ".as_bytes() {
        push_u8(&mut info, &mut in_, x);
    }
    in_ += fmt_dec(cur.humidity as u64, &mut info[in_..]);
    for &x in b"%   " {
        push_u8(&mut info, &mut in_, x);
    }
    for &x in "风速 ".as_bytes() {
        push_u8(&mut info, &mut in_, x);
    }
    in_ += fmt_dec(cur.wind as u64, &mut info[in_..]);
    for &x in "km/h ".as_bytes() {
        push_u8(&mut info, &mut in_, x);
    }
    for &x in "体感 ".as_bytes() {
        push_u8(&mut info, &mut in_, x);
    }
    let mut feels = [0u8; 12];
    let fn_ = fmt_i32(cur.feels, &mut feels);
    let k = fn_.min(info.len() - in_);
    info[in_..in_ + k].copy_from_slice(&feels[..k]);
    in_ += k;
    if in_ + 2 <= info.len() {
        info[in_] = 0xC2;
        info[in_ + 1] = 0xB0;
        in_ += 2;
    }
    draw_text(32, 220, &info[..in_], C_DIM);
    // 状态行：更新时刻 / 缓存过期 / 网络不可用
    match st {
        0 => {
            let mut ub = [0u8; 24];
            let mut un = 0;
            for &x in "更新 ".as_bytes() {
                ub[un] = x;
                un += 1;
            }
            un += fmt_hhmm(unsafe { W_TS }, &mut ub[un..]);
            draw_text(32, 248, &ub[..un], C_DIM);
        }
        1 => draw_text(32, 248, "缓存数据（可能过期）".as_bytes(), C_WARN),
        _ => draw_text(32, 248, "网络不可用".as_bytes(), C_WARN),
    }

    // 未来 5 天
    draw_text(24, 330, "未来 5 天".as_bytes(), C_DIM);
    for i in 0..5 {
        let f = weather_day(i);
        let y = 356 + i as i32 * 96;
        if y + 92 > DISPLAY_H as i32 {
            break;
        }
        // 逐日预报行（液态玻璃卡片）
        draw_glass(16, y, DISPLAY_W as i32 - 32, 84, 21);
        draw_text(32, y + 36, day_label(i).as_bytes(), C_TEXT);
        cond_icon(140, y + 42, wmo_icon(f.code));
        let label = wmo_label(f.code);
        draw_text(180, y + 36, label.as_bytes(), C_DIM);
        // 温度（高/低）
        let mut tmp = [0u8; 16];
        let mut n = 0;
        n = push_temp(&mut tmp, n, f.high);
        if n < tmp.len() {
            tmp[n] = b'/';
        }
        n += 1;
        if n > tmp.len() {
            n = tmp.len();
        }
        n = push_temp(&mut tmp, n, f.low);
        draw_text(DISPLAY_W as i32 - 160, y + 36, &tmp[..n.min(tmp.len())], C_TEXT);
    }

    draw_error_banner();
    draw_sys_nav();
}

// ==================== 点击热区 ====================

/// 「刷新」热区（当前卡右上角）。
fn refresh_hit(x: i32, y: i32) -> bool {
    x >= DISPLAY_W as i32 - 100 && x < DISPLAY_W as i32 - 16 && y >= 64 && y < 120
}

/// 城市名热区（点击切换城市）。
fn city_hit(x: i32, y: i32) -> bool {
    x >= 16 && x < 220 && y >= 64 && y < 120
}

// ==================== 主循环 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[weather] 正在启动\n");
    if !fb_init() {
        print("[weather] /dev/fb0 内存映射失败，无帧缓冲\n");
        unsafe {
            exit(1);
        }
    }
    print("[weather] 就绪\n");

    // 先渲染（此时为兜底数据），再阻塞加载真实数据（首次含 DNS+TCP，可能数秒）
    render();
    flip();
    cursor_overlay_redraw();
    kit::font_init_checked();
    render();
    flip();
    cursor_overlay_redraw();

    let st = weather_load();
    if st == 2 {
        error_ui::err_set("天气数据获取失败，请检查网络".as_bytes(), now_us());
    }
    print("[weather] 已加载 state=");
    print_dec(st as u64);
    print("\n");

    let mut dirty = true;
    loop {
        let ev = poll_input();
        if ev.moved {
            cursor_overlay_move();
        }
        if ev.nav != NavAction::None {
            print("[nav] 系统导航 → 退出\n");
            unsafe {
                exit(nav_exit_code(ev.nav));
            }
        }

        if let Some((x, y)) = ev.tap {
            if back_hit(x, y) {
                print("[weather] 返回 → 退出\n");
                break;
            }
            if error_banner_hit(x, y) {
                dirty = true;
            } else if refresh_hit(x, y) {
                weather_refresh();
                dirty = true;
            } else if city_hit(x, y) {
                weather_cycle();
                dirty = true;
            }
        }
        match ev.key {
            KeyAction::Esc => {
                print("[weather] ESC → 退出\n");
                break;
            }
            KeyAction::Char(b'r') | KeyAction::Char(b'R') => {
                weather_refresh();
                dirty = true;
            }
            KeyAction::Char(b'c') | KeyAction::Char(b'C') => {
                weather_cycle();
                dirty = true;
            }
            _ => {}
        }
        if poll_tty_esc() {
            print("[weather] ESC → 退出\n");
            break;
        }
        if dirty {
            render();
            flip();
            cursor_overlay_redraw();
            dirty = false;
        }
        sleep_ms(16);
    }

    print("[weather] 正在退出\n");
    unsafe {
        exit(0);
    }
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
        write_fd(2, b"PANIC: weather\n");
        write_fd(2, &w.buf[..w.len]);
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
