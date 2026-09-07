//! KunxOS 天气应用 JSON 提取器（纯函数，无 syscall / 无 kit 依赖，可在宿主单测）。
//!
//! 手写极简状态机，不绑 serde：单遍扫描 Open-Meteo 响应，只提取渲染需要的字段
//! （current.* 温度/湿度/体感/风速/昼夜/天气码，daily.* 5 日最高/最低温/天气码），
//! 以及 WMO weather_code → 中文状况 / 图标种类 / 桌面小组件关键字的映射。

/// 当前天气快照。
#[derive(Clone, Copy)]
pub struct CurrentWeather {
    pub temp: i32,     // 温度（取整）
    pub feels: i32,    // 体感温度
    pub humidity: i32, // 相对湿度 %
    pub wind: i32,     // 风速 km/h
    pub code: u16,     // WMO weather_code
    pub is_day: bool,
}

impl CurrentWeather {
    pub const fn empty() -> Self {
        CurrentWeather { temp: 0, feels: 0, humidity: 0, wind: 0, code: 0, is_day: true }
    }
}

/// 单日预报。
#[derive(Clone, Copy)]
pub struct DayForecast {
    pub code: u16,
    pub high: i32,
    pub low: i32,
}

impl DayForecast {
    pub const fn empty() -> Self {
        DayForecast { code: 0, high: 0, low: 0 }
    }
}

// ==================== 上下文 / 键 ID ====================

/// 上下文栈取值：对象 / "current" 对象 / "daily" 对象 / 三种 daily 数组。
const CX_OBJ: u8 = 0;
const CX_CUR: u8 = 1;
const CX_DAY: u8 = 2;
const CX_A_WC: u8 = 3;
const CX_A_MAX: u8 = 4;
const CX_A_MIN: u8 = 5;

/// 键 ID。
const KEY_NONE: u8 = 0;
const KEY_CURRENT: u8 = 1;
const KEY_DAILY: u8 = 2;
const KEY_TEMP: u8 = 3;
const KEY_HUM: u8 = 4;
const KEY_FEELS: u8 = 5;
const KEY_ISDAY: u8 = 6;
const KEY_WC: u8 = 7;
const KEY_WIND: u8 = 8;
const KEY_MAX: u8 = 9;
const KEY_MIN: u8 = 10;

fn match_key(s: &[u8]) -> u8 {
    if s == b"current" {
        KEY_CURRENT
    } else if s == b"daily" {
        KEY_DAILY
    } else if s == b"temperature_2m" {
        KEY_TEMP
    } else if s == b"relative_humidity_2m" {
        KEY_HUM
    } else if s == b"apparent_temperature" {
        KEY_FEELS
    } else if s == b"is_day" {
        KEY_ISDAY
    } else if s == b"weather_code" {
        KEY_WC
    } else if s == b"wind_speed_10m" {
        KEY_WIND
    } else if s == b"temperature_2m_max" {
        KEY_MAX
    } else if s == b"temperature_2m_min" {
        KEY_MIN
    } else {
        KEY_NONE
    }
}

// ==================== 词法 ====================

/// 解析字符串字面量（跳过转义），返回内容区间 [s, e)。
fn parse_string(b: &[u8], start: usize) -> (usize, usize) {
    let mut j = start + 1;
    while j < b.len() {
        if b[j] == b'\\' {
            j += 2;
            continue;
        }
        if b[j] == b'"' {
            return (start + 1, j);
        }
        j += 1;
    }
    (start + 1, b.len())
}

/// 解析数字，返回 (负号, 整数部分, 小数首位, 结束下标)。
fn parse_num(b: &[u8], start: usize) -> (bool, u64, u32, usize) {
    let mut i = start;
    let mut neg = false;
    if i < b.len() && b[i] == b'-' {
        neg = true;
        i += 1;
    }
    let mut int: u64 = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        int = int.saturating_mul(10).saturating_add((b[i] - b'0') as u64);
        i += 1;
    }
    let mut frac: u32 = 0;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        if i < b.len() && b[i].is_ascii_digit() {
            frac = (b[i] - b'0') as u32;
        }
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    (neg, int, frac, i)
}

/// 四舍五入到整数：26.7→27，-5.7→-6。
fn round(neg: bool, int: u64, frac: u32) -> i32 {
    let v = int.min(i32::MAX as u64) as i64;
    let up = (frac >= 5) as i64;
    if neg {
        (-v - up) as i32
    } else {
        (v + up) as i32
    }
}

// ==================== 解析 ====================

/// 捕获一个数值到输出模型。
fn capture(
    ctx: u8,
    key: u8,
    neg: bool,
    int: u64,
    frac: u32,
    cur: &mut CurrentWeather,
    days: &mut [DayForecast; 5],
    got_cur: &mut bool,
    got_day: &mut bool,
    idx: &mut [usize; 3], // [weather_code, max, min]
) {
    match ctx {
        CX_CUR => match key {
            KEY_TEMP => {
                cur.temp = round(neg, int, frac);
                *got_cur = true;
            }
            KEY_FEELS => cur.feels = round(neg, int, frac),
            KEY_HUM => cur.humidity = round(neg, int, frac),
            KEY_WIND => cur.wind = round(neg, int, frac),
            KEY_ISDAY => cur.is_day = int != 0,
            KEY_WC => cur.code = int.min(u16::MAX as u64) as u16,
            _ => {}
        },
        CX_A_WC => {
            if idx[0] < 5 {
                days[idx[0]].code = int.min(u16::MAX as u64) as u16;
                idx[0] += 1;
            }
        }
        CX_A_MAX => {
            if idx[1] < 5 {
                days[idx[1]].high = round(neg, int, frac);
                idx[1] += 1;
                *got_day = true;
            }
        }
        CX_A_MIN => {
            if idx[2] < 5 {
                days[idx[2]].low = round(neg, int, frac);
                idx[2] += 1;
            }
        }
        _ => {}
    }
}

/// 单遍扫描 JSON，把所需字段提取到 `cur` / `days`。
/// 返回是否取到核心字段（当前温度 + 至少一天预报）；失败时输出可能部分填充，
/// 调用方应使用临时副本、成功后再提交。
pub fn json_parse(b: &[u8], cur: &mut CurrentWeather, days: &mut [DayForecast; 5]) -> bool {
    let mut got_cur = false;
    let mut got_day = false;
    let mut idx = [0usize; 3];

    let mut stack = [CX_OBJ; 8];
    let mut sp = 0usize;
    let mut key: u8 = KEY_NONE;
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        match c {
            b' ' | b'\t' | b'\r' | b'\n' | b',' | b':' => i += 1,
            b'{' => {
                let ctx = if key == KEY_CURRENT {
                    CX_CUR
                } else if key == KEY_DAILY {
                    CX_DAY
                } else {
                    CX_OBJ
                };
                if sp < stack.len() {
                    stack[sp] = ctx;
                    sp += 1;
                }
                key = KEY_NONE;
                i += 1;
            }
            b'}' => {
                if sp > 0 {
                    sp -= 1;
                }
                i += 1;
            }
            b'[' => {
                let top = if sp > 0 { stack[sp - 1] } else { CX_OBJ };
                let ctx = if top == CX_DAY {
                    match key {
                        KEY_WC => CX_A_WC,
                        KEY_MAX => CX_A_MAX,
                        KEY_MIN => CX_A_MIN,
                        _ => CX_OBJ,
                    }
                } else {
                    CX_OBJ
                };
                if sp < stack.len() {
                    stack[sp] = ctx;
                    sp += 1;
                }
                key = KEY_NONE;
                i += 1;
            }
            b']' => {
                if sp > 0 {
                    sp -= 1;
                }
                i += 1;
            }
            b'"' => {
                let (s, e) = parse_string(b, i);
                // 后视一个冒号：是键还是字符串值（daily.time 等字符串值不需要）。
                // 注意 e 是右引号下标，后视从 e+1 开始。
                let mut k = e + 1;
                while k < b.len()
                    && (b[k] == b' ' || b[k] == b'\t' || b[k] == b'\r' || b[k] == b'\n')
                {
                    k += 1;
                }
                if k < b.len() && b[k] == b':' {
                    key = match_key(&b[s..e]);
                }
                i = e + 1;
            }
            b'-' | b'0'..=b'9' => {
                let (neg, int, frac, next) = parse_num(b, i);
                let top = if sp > 0 { stack[sp - 1] } else { CX_OBJ };
                capture(top, key, neg, int, frac, cur, days, &mut got_cur, &mut got_day, &mut idx);
                key = KEY_NONE;
                i = next;
            }
            b't' => {
                i += 4;
                key = KEY_NONE;
            }
            b'f' => {
                i += 5;
                key = KEY_NONE;
            }
            b'n' => {
                i += 4;
                key = KEY_NONE;
            }
            _ => i += 1,
        }
    }
    got_cur && got_day
}

// ==================== WMO 天气码 → 展示 ====================

/// 图标种类：0 太阳 / 1 云 / 2 雨 / 3 雪 / 4 雷 / 5 雾。
pub fn wmo_icon(code: u16) -> u8 {
    match code {
        0 => 0,
        1..=3 | 45 | 48 => 1,
        51..=67 | 80..=82 => 2,
        95..=99 => 4,
        71..=77 | 85 | 86 => 3,
        _ => 1,
    }
}

pub fn wmo_label(code: u16) -> &'static str {
    match code {
        0 => "晴",
        1 | 2 => "多云",
        3 => "阴",
        45 | 48 => "雾",
        51 | 53 | 55 | 56 | 57 => "小雨",
        61 | 80 | 81 | 82 => "小雨",
        63 => "中雨",
        65 => "大雨",
        66 | 67 => "冻雨",
        71 | 77 => "小雪",
        73 | 85 | 86 => "中雪",
        75 => "大雪",
        95 | 96 | 99 => "雷雨",
        _ => "晴",
    }
}

/// launcher 桌面小组件状况关键字（其 cond_icon 按 rain/snow/cloud 子串匹配）。
pub fn wmo_widget(code: u16) -> &'static [u8] {
    match wmo_icon(code) {
        3 => b"snow",
        2 | 4 => b"rain",
        1 | 5 => b"cloudy",
        _ => b"sunny",
    }
}
