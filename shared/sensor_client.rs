//! 应用侧传感器 / 定位客户端：读取 sensord / positiond 的状态文件。
//!
//! 自包含（no_std、裸 syscall、无堆），任何应用以
//! `#[path = "../../../shared/sensor_client.rs"] mod sensor_client;` 引入。
//!
//! - [`sensor_state`]：解析 `/var/run/sensord.state`
//!   （ORIENT / QUAT / HEADING / STEPS / RAISE / MODE / ORIENTATION）
//! - [`position_state`]：解析 `/var/run/positiond.state`
//!   （FIX lat×100000 lon×100000 alt_cm spd×10 crs×10 sats seq / NOFIX / UTC）
//! - [`orientation_name`]：屏幕方向名（自动旋转）
//!
//! 状态文件不存在时返回 `valid = false`，调用方按「传感器离线」处理。

use core::arch::asm;

// ==================== 系统调用 ====================

const SYS_READ: usize = 63;
const SYS_WRITE: usize = 64;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;

const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
const O_RDONLY: usize = 0;

#[inline(always)]
unsafe fn syscall3(n: usize, a1: usize, a2: usize, a3: usize) -> usize {
    let ret: usize;
    asm!(
        "svc #0",
        in("x8") n,
        inlateout("x0") a1 => ret,
        in("x1") a2,
        in("x2") a3,
        options(nostack)
    );
    ret
}

#[inline(always)]
unsafe fn syscall1(n: usize, a1: usize) -> usize {
    let ret: usize;
    asm!("svc #0", in("x8") n, inlateout("x0") a1 => ret, options(nostack));
    ret
}

static mut PATH_BUF: [u8; 128] = [0u8; 128];

/// 读取整个文件到 `buf`，返回字节数（失败返回 0）。
fn read_file(path: &str, buf: &mut [u8]) -> usize {
    unsafe {
        let n = path.len().min(PATH_BUF.len() - 1);
        PATH_BUF[..n].copy_from_slice(&path.as_bytes()[..n]);
        PATH_BUF[n] = 0;
        let fd = syscall3(SYS_OPENAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, O_RDONLY) as isize;
        if fd < 0 {
            return 0;
        }
        let r = syscall3(SYS_READ, fd as usize, buf.as_mut_ptr() as usize, buf.len()) as isize;
        syscall1(SYS_CLOSE, fd as usize);
        if r <= 0 { 0 } else { r as usize }
    }
}

// ==================== 解析辅助 ====================

/// 跳过空白与换行。
fn skip_ws(d: &[u8], i: &mut usize) {
    while *i < d.len() && (d[*i] == b' ' || d[*i] == b'\n' || d[*i] == b'\r' || d[*i] == b'\t') {
        *i += 1;
    }
}

/// 取下一个空白分隔的字段。
fn next_field<'a>(d: &'a [u8], i: &mut usize) -> &'a [u8] {
    skip_ws(d, i);
    let s = *i;
    while *i < d.len() && d[*i] != b' ' && d[*i] != b'\n' && d[*i] != b'\r' && d[*i] != b'\t' {
        *i += 1;
    }
    &d[s..*i]
}

fn parse_i64(f: &[u8]) -> Option<i64> {
    if f.is_empty() {
        return None;
    }
    let mut neg = false;
    let mut i = 0usize;
    if f[0] == b'-' {
        neg = true;
        i = 1;
    }
    let mut v: i64 = 0;
    let mut any = false;
    while i < f.len() {
        let c = f[i];
        if !(b'0'..=b'9').contains(&c) {
            return None;
        }
        any = true;
        v = v * 10 + (c - b'0') as i64;
        i += 1;
    }
    if !any {
        return None;
    }
    Some(if neg { -v } else { v })
}

// ==================== 数据结构 ====================

/// 屏幕方向（sensord 计算；与 Android 约定一致）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenOrientation {
    Portrait = 0,
    LandscapeLeft = 1,
    ReversePortrait = 2,
    LandscapeRight = 3,
}

impl ScreenOrientation {
    pub fn from_u8(v: u8) -> ScreenOrientation {
        match v {
            1 => ScreenOrientation::LandscapeLeft,
            2 => ScreenOrientation::ReversePortrait,
            3 => ScreenOrientation::LandscapeRight,
            _ => ScreenOrientation::Portrait,
        }
    }
}

/// 传感器状态（sensord 最新融合输出）。
#[derive(Clone, Copy, Debug)]
pub struct SensorState {
    pub valid: bool,
    /// 欧拉角（度，整数）。
    pub roll: i32,
    pub pitch: i32,
    pub yaw: i32,
    /// 四元数（×10000）。
    pub q: [i32; 4],
    /// 航向（0-360，整数度）。
    pub heading: i32,
    pub steps: u32,
    pub raises: u32,
    /// "game" / "geo"。
    pub mode: [u8; 4],
    pub orientation: ScreenOrientation,
}

/// 定位状态（positiond 最新输出）。
#[derive(Clone, Copy, Debug)]
pub struct PositionState {
    pub valid: bool,
    pub has_fix: bool,
    /// 纬度 / 经度（WGS84，×100000）。
    pub lat: i64,
    pub lon: i64,
    /// 海拔（厘米）。
    pub alt_cm: i64,
    /// 速度（km/h ×10）。
    pub speed_x10: i64,
    /// 航向（度 ×10）。
    pub course_x10: i64,
    pub sats: u32,
    pub seq: u32,
    /// UTC (时, 分, 秒)。
    pub utc: (u32, u32, u32),
}

impl Default for SensorState {
    fn default() -> Self {
        SensorState {
            valid: false,
            roll: 0,
            pitch: 0,
            yaw: 0,
            q: [10000, 0, 0, 0],
            heading: 0,
            steps: 0,
            raises: 0,
            mode: [0; 4],
            orientation: ScreenOrientation::Portrait,
        }
    }
}

impl Default for PositionState {
    fn default() -> Self {
        PositionState {
            valid: false,
            has_fix: false,
            lat: 0,
            lon: 0,
            alt_cm: 0,
            speed_x10: 0,
            course_x10: 0,
            sats: 0,
            seq: 0,
            utc: (0, 0, 0),
        }
    }
}

/// 屏幕方向名（UTF-8）。
pub fn orientation_name(o: ScreenOrientation) -> &'static [u8] {
    match o {
        ScreenOrientation::Portrait => "竖屏".as_bytes(),
        ScreenOrientation::LandscapeLeft => "横屏左".as_bytes(),
        ScreenOrientation::ReversePortrait => "倒竖".as_bytes(),
        ScreenOrientation::LandscapeRight => "横屏右".as_bytes(),
    }
}

// ==================== 读取入口 ====================

/// 读取传感器状态（每次调用重新读文件；文件缺失 → `valid = false`）。
pub fn sensor_state() -> SensorState {
    let mut st = SensorState::default();
    let mut buf = [0u8; 512];
    let n = read_file("/var/run/sensord.state", &mut buf);
    if n == 0 {
        return st;
    }
    let d = &buf[..n];
    let mut i = 0usize;
    while i < d.len() {
        let key = next_field(d, &mut i);
        if key.is_empty() {
            break;
        }
        match key {
            b"ORIENT" => {
                if let (Some(r), Some(p), Some(y)) =
                    (parse_i64(next_field(d, &mut i)), parse_i64(next_field(d, &mut i)), parse_i64(next_field(d, &mut i)))
                {
                    st.roll = r as i32;
                    st.pitch = p as i32;
                    st.yaw = y as i32;
                }
            }
            b"QUAT" => {
                let vals = [
                    parse_i64(next_field(d, &mut i)),
                    parse_i64(next_field(d, &mut i)),
                    parse_i64(next_field(d, &mut i)),
                    parse_i64(next_field(d, &mut i)),
                ];
                if let (Some(w), Some(x), Some(y), Some(z)) = (vals[0], vals[1], vals[2], vals[3]) {
                    st.q = [w as i32, x as i32, y as i32, z as i32];
                }
            }
            b"HEADING" => {
                if let Some(h) = parse_i64(next_field(d, &mut i)) {
                    st.heading = h as i32;
                }
            }
            b"STEPS" => {
                if let Some(v) = parse_i64(next_field(d, &mut i)) {
                    st.steps = v as u32;
                }
            }
            b"RAISE" => {
                if let Some(v) = parse_i64(next_field(d, &mut i)) {
                    st.raises = v as u32;
                }
            }
            b"MODE" => {
                let m = next_field(d, &mut i);
                let l = m.len().min(4);
                st.mode[..l].copy_from_slice(&m[..l]);
            }
            b"ORIENTATION" => {
                if let Some(v) = parse_i64(next_field(d, &mut i)) {
                    st.orientation = ScreenOrientation::from_u8(v as u8);
                }
            }
            _ => {
                // 未知键：跳过该行。
                while i < d.len() && d[i] != b'\n' {
                    i += 1;
                }
            }
        }
    }
    st.valid = true;
    st
}

/// 读取定位状态（文件缺失 → `valid = false`）。
pub fn position_state() -> PositionState {
    let mut st = PositionState::default();
    let mut buf = [0u8; 256];
    let n = read_file("/var/run/positiond.state", &mut buf);
    if n == 0 {
        return st;
    }
    let d = &buf[..n];
    let mut i = 0usize;
    while i < d.len() {
        let key = next_field(d, &mut i);
        if key.is_empty() {
            break;
        }
        match key {
            b"FIX" => {
                let vals = [
                    parse_i64(next_field(d, &mut i)),
                    parse_i64(next_field(d, &mut i)),
                    parse_i64(next_field(d, &mut i)),
                    parse_i64(next_field(d, &mut i)),
                    parse_i64(next_field(d, &mut i)),
                    parse_i64(next_field(d, &mut i)),
                    parse_i64(next_field(d, &mut i)),
                ];
                if let (Some(lat), Some(lon), Some(alt), Some(spd), Some(crs), Some(sats), Some(seq)) =
                    (vals[0], vals[1], vals[2], vals[3], vals[4], vals[5], vals[6])
                {
                    st.lat = lat;
                    st.lon = lon;
                    st.alt_cm = alt;
                    st.speed_x10 = spd;
                    st.course_x10 = crs;
                    st.sats = sats as u32;
                    st.seq = seq as u32;
                    st.has_fix = true;
                }
            }
            b"UTC" => {
                let h = parse_i64(next_field(d, &mut i)).unwrap_or(0) as u32;
                let m = parse_i64(next_field(d, &mut i)).unwrap_or(0) as u32;
                let s = parse_i64(next_field(d, &mut i)).unwrap_or(0) as u32;
                st.utc = (h, m, s);
            }
            b"NOFIX" => {
                st.has_fix = false;
            }
            _ => {
                while i < d.len() && d[i] != b'\n' {
                    i += 1;
                }
            }
        }
    }
    st.valid = true;
    st
}
