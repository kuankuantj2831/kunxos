//! 姿态融合：Mahony 互补滤波（AHRS），纯 Rust 实现。
//!
//! 输入：
//! - 陀螺仪角速度（dps，体坐标系）
//! - 加速度计（g，体坐标系）
//! - 磁力计（µT，体坐标系，可选）
//!
//! 输出：
//! - [`Quat`]：单位四元数（体→世界旋转）
//! - [`Euler`]：欧拉角（度，ZYX 顺序）
//! - [`RotationVector`]：Android 风格旋转矢量（x,y,z,w），
//!   无磁力计 = 游戏旋转矢量（game rotation vector），
//!   有磁力计 = 地理旋转矢量（geographic rotation vector）。
//!
//! 算法：Mahony et al. (2008) 《An efficient orientation filter for
//! inertial and inertial/magnetic sensor arrays》。陀螺仪积分 + 加速度计
//! （和磁力计）误差的比例-积分反馈修正：
//!
//! ```text
//!   q̇ = 0.5 · q ⊗ (0, ω)          # 陀螺积分
//!   e  = a × v  (+ m × w)         # 测量与估计方向的叉积误差
//!   ω' = ω + Kp·e + Ki·∫e·dt      # 修正角速度
//! ```
//!
//! 增益：`Kp`（比例，收敛速度）默认 0.5，`Ki`（积分，消除稳态漂移）
//! 默认 0.05；游戏/UI 场景可调大，噪声大可调小。

use crate::math;

// ==================== 基础类型 ====================

/// 三维向量（f32）。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vec3 {
    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Vec3 { x, y, z }
    }
    /// 叉积。
    pub fn cross(self, o: Vec3) -> Vec3 {
        Vec3 {
            x: self.y * o.z - self.z * o.y,
            y: self.z * o.x - self.x * o.z,
            z: self.x * o.y - self.y * o.x,
        }
    }
    /// 点积。
    pub fn dot(self, o: Vec3) -> f32 {
        self.x * o.x + self.y * o.y + self.z * o.z
    }
    /// 模长。
    pub fn norm(self) -> f32 {
        math::sqrt(self.dot(self))
    }
    /// 归一化（模长为 0 时原样返回）。
    pub fn normalized(self) -> Vec3 {
        let n = self.norm();
        if n <= 0.0 || !n.is_finite() {
            self
        } else {
            Vec3::new(self.x / n, self.y / n, self.z / n)
        }
    }
}

/// 四元数（w + xi + yj + zk），单位化后表示旋转。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Quat {
    pub w: f32,
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Quat {
    pub const fn new(w: f32, x: f32, y: f32, z: f32) -> Self {
        Quat { w, x, y, z }
    }
    /// 单位四元数（无旋转）。
    pub const fn identity() -> Self {
        Quat { w: 1.0, x: 0.0, y: 0.0, z: 0.0 }
    }
    /// 四元数乘法。
    pub fn mul(self, o: Quat) -> Quat {
        Quat {
            w: self.w * o.w - self.x * o.x - self.y * o.y - self.z * o.z,
            x: self.w * o.x + self.x * o.w + self.y * o.z - self.z * o.y,
            y: self.w * o.y - self.x * o.z + self.y * o.w + self.z * o.x,
            z: self.w * o.z + self.x * o.y - self.y * o.x + self.z * o.w,
        }
    }
    /// 共轭（逆）。
    pub fn conj(self) -> Quat {
        Quat::new(self.w, -self.x, -self.y, -self.z)
    }
    /// 归一化。
    pub fn normalized(self) -> Quat {
        let n = math::sqrt(self.w * self.w + self.x * self.x + self.y * self.y + self.z * self.z);
        if n <= 0.0 || !n.is_finite() {
            Quat::identity()
        } else {
            Quat::new(self.w / n, self.x / n, self.y / n, self.z / n)
        }
    }
    /// 用四元数旋转向量（体→世界）。
    pub fn rotate(self, v: Vec3) -> Vec3 {
        let qv = Vec3::new(self.x, self.y, self.z);
        let t = qv.cross(v).scale(2.0);
        Vec3::new(
            v.x + self.w * t.x + qv.cross(t).x,
            v.y + self.w * t.y + qv.cross(t).y,
            v.z + self.w * t.z + qv.cross(t).z,
        )
    }
    /// 用四元数逆旋转向量（世界→体）。
    pub fn rotate_inv(self, v: Vec3) -> Vec3 {
        self.conj().rotate(v)
    }
}

impl Vec3 {
    /// 标量乘法。
    pub fn scale(self, s: f32) -> Vec3 {
        Vec3::new(self.x * s, self.y * s, self.z * s)
    }
    /// 向量加法。
    pub fn add(self, o: Vec3) -> Vec3 {
        Vec3::new(self.x + o.x, self.y + o.y, self.z + o.z)
    }
}

/// 欧拉角（度）。约定：ZYX 顺序（yaw → pitch → roll），
/// roll 绕 X（横滚）、pitch 绕 Y（俯仰）、yaw 绕 Z（航向/方位角）。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Euler {
    pub roll: f32,
    pub pitch: f32,
    pub yaw: f32,
}

/// Android 风格旋转矢量：`(x, y, z, w)`，其中 `w = cos(θ/2)`，
/// `(x,y,z) = sin(θ/2)·轴`。游戏旋转矢量 = 无磁力计版本；
/// 地理旋转矢量 = 含磁力计版本（航向相对磁北）。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RotationVector {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub w: f32,
}

// ==================== Mahony 滤波器 ====================

/// Mahony AHRS 互补滤波器。
pub struct MahonyFilter {
    /// 当前姿态四元数（体→世界）。
    pub q: Quat,
    /// 比例增益（收敛速度，典型 0.1 ~ 2.0）。
    pub kp: f32,
    /// 积分增益（消除陀螺零偏漂移，典型 0 ~ 0.2）。
    pub ki: f32,
    /// 积分误差累积。
    integral: Vec3,
    /// 上次更新时间（秒，用于 dt 积分项）。
    last_dt: f32,
}

impl Default for MahonyFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl MahonyFilter {
    /// 创建滤波器（Kp = 0.5，Ki = 0.05，常用起步值）。
    pub fn new() -> Self {
        MahonyFilter {
            q: Quat::identity(),
            kp: 0.5,
            ki: 0.05,
            integral: Vec3::default(),
            last_dt: 0.0,
        }
    }

    /// 配置增益。
    pub fn set_gains(&mut self, kp: f32, ki: f32) {
        self.kp = kp;
        self.ki = ki;
    }

    /// 重置为初始姿态（可选给定初始四元数）。
    pub fn reset(&mut self) {
        self.q = Quat::identity();
        self.integral = Vec3::default();
        self.last_dt = 0.0;
    }

    /// 核心一步：给定修正误差向量 `err`（测量叉积估计），
    /// 计算修正角速度并积分四元数。`gyro` 单位为 dps（度/秒），
    /// 内部转换为 rad/s 参与四元数微分。
    fn step(&mut self, gyro: Vec3, err: Vec3, dt: f32) {
        // 误差积分（用于消除陀螺零偏）——只在误差较小时积分，避免发散。
        let e_norm = err.norm();
        if e_norm < 0.5 {
            self.integral = self.integral.add(err.scale(self.ki * dt));
            // 积分项限幅，防止长时间大误差时积分爆炸。
            let lim = 1.0;
            self.integral = Vec3::new(
                math::clamp(self.integral.x, -lim, lim),
                math::clamp(self.integral.y, -lim, lim),
                math::clamp(self.integral.z, -lim, lim),
            );
        }
        // 修正角速度（rad/s）= 陀螺（dps→rad/s）+ 比例项 + 积分项。
        let gx = math::to_rad(gyro.x) + self.kp * err.x + self.integral.x;
        let gy = math::to_rad(gyro.y) + self.kp * err.y + self.integral.y;
        let gz = math::to_rad(gyro.z) + self.kp * err.z + self.integral.z;
        // 四元数微分：q̇ = 0.5 · q ⊗ (0, ω)，一阶积分 + 归一化。
        let dq = Quat::new(0.0, gx, gy, gz).mul(self.q).scale_half();
        self.q = Quat::new(
            self.q.w + dq.w * dt,
            self.q.x + dq.x * dt,
            self.q.y + dq.y * dt,
            self.q.z + dq.z * dt,
        )
        .normalized();
        self.last_dt = dt;
    }

    /// IMU 模式更新（陀螺 + 加速度，无磁力计）：
    /// 输出**游戏旋转矢量**。`gyro` 单位 dps，`accel` 单位 g。
    pub fn update_imu(&mut self, gyro: Vec3, accel: Vec3, dt: f32) {
        let a = accel.normalized();
        // 由四元数估计的重力方向（世界 (0,0,1) 旋转到体坐标系）。
        let v = self
            .q
            .rotate_inv(Vec3::new(0.0, 0.0, 1.0));
        // 误差 = a × v（测量与估计的叉积）。
        let err = a.cross(v);
        self.step(gyro, err, dt);
    }

    /// AHRS 模式更新（陀螺 + 加速度 + 磁力计）：
    /// 输出**地理旋转矢量**（航向相对磁北）。`mag` 单位 µT。
    pub fn update_mag(&mut self, gyro: Vec3, accel: Vec3, mag: Vec3, dt: f32) {
        let a = accel.normalized();
        let m = mag.normalized();
        let q = self.q;

        // 1) 加速度误差（同 IMU 模式）。
        let v = q.rotate_inv(Vec3::new(0.0, 0.0, 1.0));
        let err_a = a.cross(v);

        // 2) 磁力计误差：把测量旋转到世界系求参考方向 b，再旋转回体系
        //    得到估计方向 w，误差 = m × w。
        let (qw, qx, qy, qz) = (q.w, q.x, q.y, q.z);
        let hx = 2.0 * (mx_world(m.x, m.y, m.z, qw, qx, qy, qz, 0));
        let hy = 2.0 * (my_world(m.x, m.y, m.z, qw, qx, qy, qz, 0));
        let hz = 2.0 * (mz_world(m.x, m.y, m.z, qw, qx, qy, qz, 0));
        // 世界系磁场参考：水平分量 bx（取水平模长，消除磁偏角影响），垂直 bz。
        let bx = math::sqrt(hx * hx + hy * hy);
        let bz = hz;
        // 估计的体坐标系磁场方向 w。
        let wx = 2.0 * (bx * (0.5 - qy * qy - qz * qz) + bz * (qx * qz - qw * qy));
        let wy = 2.0 * (bx * (qx * qy - qw * qz) + bz * (qw * qx + qy * qz));
        let wz = 2.0 * (bx * (qx * qz + qw * qy) + bz * (0.5 - qx * qx - qy * qy));
        let w = Vec3::new(wx, wy, wz);
        let err_m = m.cross(w);

        // 3) 合并误差。
        let err = err_a.add(err_m);
        self.step(gyro, err, dt);
    }

    /// 欧拉角（度，ZYX：roll/pitch/yaw）。
    pub fn euler(&self) -> Euler {
        quat_to_euler(self.q)
    }

    /// 航向角（度，0-360，绕世界 Z 轴）。
    pub fn heading_deg(&self) -> f32 {
        let e = self.euler();
        let mut h = e.yaw;
        while h < 0.0 {
            h += 360.0;
        }
        while h >= 360.0 {
            h -= 360.0;
        }
        h
    }

    /// 游戏旋转矢量（无磁力计姿态）。
    pub fn game_rotation_vector(&self) -> RotationVector {
        quat_to_rotation_vector(self.q)
    }

    /// 地理旋转矢量（含磁力计姿态）。
    pub fn geographic_rotation_vector(&self) -> RotationVector {
        quat_to_rotation_vector(self.q)
    }
}

impl Quat {
    /// 四元数除以 2（用于微分项）。
    fn scale_half(self) -> Quat {
        Quat::new(self.w * 0.5, self.x * 0.5, self.y * 0.5, self.z * 0.5)
    }
}

// ==================== 旋转 → 世界系磁场（辅助） ====================

/// `h = R(q)·m` 的 x 分量（2 倍系数由调用方处理）。
#[inline]
fn mx_world(mx: f32, my: f32, mz: f32, qw: f32, qx: f32, qy: f32, qz: f32, _i: i32) -> f32 {
    mx * (0.5 - qy * qy - qz * qz) + my * (qx * qy - qw * qz) + mz * (qx * qz + qw * qy)
}
/// `h = R(q)·m` 的 y 分量。
#[inline]
fn my_world(mx: f32, my: f32, mz: f32, qw: f32, qx: f32, qy: f32, qz: f32, _i: i32) -> f32 {
    mx * (qx * qy + qw * qz) + my * (0.5 - qx * qx - qz * qz) + mz * (qy * qz - qw * qx)
}
/// `h = R(q)·m` 的 z 分量。
#[inline]
fn mz_world(mx: f32, my: f32, mz: f32, qw: f32, qx: f32, qy: f32, qz: f32, _i: i32) -> f32 {
    mx * (qx * qz - qw * qy) + my * (qy * qz + qw * qx) + mz * (0.5 - qx * qx - qy * qy)
}

// ==================== 换算 ====================

/// 四元数 → 欧拉角（度，ZYX 顺序）。
pub fn quat_to_euler(q: Quat) -> Euler {
    let (w, x, y, z) = (q.w, q.x, q.y, q.z);
    let roll = math::atan2(2.0 * (w * x + y * z), 1.0 - 2.0 * (x * x + y * y));
    let pitch = math::asin(math::clamp(2.0 * (w * y - z * x), -1.0, 1.0));
    let yaw = math::atan2(2.0 * (w * z + x * y), 1.0 - 2.0 * (y * y + z * z));
    Euler {
        roll: math::to_deg(roll),
        pitch: math::to_deg(pitch),
        yaw: math::to_deg(yaw),
    }
}

/// 四元数 → Android 旋转矢量（x,y,z,w；即四元数本身）。
pub fn quat_to_rotation_vector(q: Quat) -> RotationVector {
    RotationVector { x: q.x, y: q.y, z: q.z, w: q.w }
}

/// 欧拉角（度）→ 四元数（ZYX 顺序，roll→pitch→yaw）。
pub fn euler_to_quat(roll: f32, pitch: f32, yaw: f32) -> Quat {
    let (r, p, y) = (math::to_rad(roll), math::to_rad(pitch), math::to_rad(yaw));
    let (cr, sr) = (math::cos(r * 0.5), math::sin(r * 0.5));
    let (cp, sp) = (math::cos(p * 0.5), math::sin(p * 0.5));
    let (cy, sy) = (math::cos(y * 0.5), math::sin(y * 0.5));
    Quat::new(
        cr * cp * cy + sr * sp * sy,
        sr * cp * cy - cr * sp * sy,
        cr * sp * cy + sr * cp * sy,
        cr * cp * sy - sr * sp * cy,
    )
}

/// 由四元数旋转重力向量 (0,0,1) 得到体坐标系重力方向（即静止加速度计读数方向）。
pub fn gravity_in_body(q: Quat) -> Vec3 {
    q.rotate_inv(Vec3::new(0.0, 0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use crate::math::{self, to_deg, to_rad};

    #[test]
    fn test_identity() {
        let q = Quat::identity();
        let e = quat_to_euler(q);
        assert!(e.roll.abs() < 1e-4 && e.pitch.abs() < 1e-4 && e.yaw.abs() < 1e-4);
    }

    #[test]
    fn test_euler_roundtrip() {
        for (r, p, y) in [(0.0f32, 0.0, 0.0), (30.0, 0.0, 0.0), (0.0, 45.0, 0.0), (0.0, 0.0, 90.0), (10.0, 20.0, 30.0)] {
            let q = euler_to_quat(r, p, y);
            let e = quat_to_euler(q);
            assert!((e.roll - r).abs() < 1.0, "roll {}/{}", e.roll, r);
            assert!((e.pitch - p).abs() < 1.0, "pitch {}/{}", e.pitch, p);
            assert!((e.yaw - y).abs() < 1.0, "yaw {}/{}", e.yaw, y);
        }
    }

    #[test]
    fn test_update_imu_converges() {
        // 绕 X 轴 90°/s 旋转：roll 可被加速度计观测，滤波应收敛。
        let mut f = MahonyFilter::new();
        f.kp = 0.5;
        f.ki = 0.0;
        let dt = 0.01;
        let mut t = 0.0f32;
        for i in 0..500 {
            let q = euler_to_quat(90.0 * t, 0.0, 0.0);
            let accel = gravity_in_body(q);
            let gyro = Vec3::new(90.0, 0.0, 0.0); // dps
            f.update_imu(gyro, accel, dt);
            t += dt;

        }
        let e = f.euler();
        // 450° ≡ 90°（wrap）。
        assert!((e.roll - 90.0).abs() < 3.0, "roll {} (wrap ok)", e.roll);
        assert!(e.pitch.abs() < 3.0, "pitch {}", e.pitch);
        assert!(e.yaw.abs() < 3.0, "yaw {}", e.yaw);
    }

    #[test]
    fn test_gravity_matches_sim() {
        // 静止平放（roll=pitch=yaw=0）：加速度计应读到 (0,0,+1)。
        let q = Quat::identity();
        let g = gravity_in_body(q);
        assert!((g.x).abs() < 1e-5 && (g.y).abs() < 1e-5 && (g.z - 1.0).abs() < 1e-5);
        // roll 90°：读到 (0,1,0)。
        let q = euler_to_quat(90.0, 0.0, 0.0);
        let g = gravity_in_body(q);
        assert!((g.x).abs() < 1e-3 && (g.y - 1.0).abs() < 1e-3 && (g.z).abs() < 1e-3);
        // pitch 90°：读到 (-1,0,0)。
        let q = euler_to_quat(0.0, 90.0, 0.0);
        let g = gravity_in_body(q);
        assert!((g.x + 1.0).abs() < 1e-3 && (g.y).abs() < 1e-3 && (g.z).abs() < 1e-3);
    }

    #[test]
    fn test_heading() {
        let mut f = MahonyFilter::new();
        f.q = euler_to_quat(0.0, 0.0, 90.0);
        let h = f.heading_deg();
        assert!((h - 90.0).abs() < 0.5, "heading {h}");
        f.q = euler_to_quat(0.0, 0.0, -90.0);
        let h = f.heading_deg();
        assert!((h - 270.0).abs() < 0.5, "heading {h}");
    }

    #[test]
    fn test_rotation_vector() {
        let mut f = MahonyFilter::new();
        f.q = euler_to_quat(30.0, 0.0, 0.0);
        let rv = f.game_rotation_vector();
        let n = math::sqrt(rv.x * rv.x + rv.y * rv.y + rv.z * rv.z + rv.w * rv.w);
        assert!((n - 1.0).abs() < 1e-5);
    }

    #[test]
    fn test_update_mag_yaw_converges() {
        // 只有磁力计（无陀螺/加速度时），yaw 应收敛到磁北方向。
        let mut f = MahonyFilter::new();
        f.kp = 2.0;
        f.ki = 0.0;
        // 真实姿态：yaw = 60°，磁北世界系 (0.4, 0, 0.6)（水平分量朝 +x）。
        let q_true = euler_to_quat(0.0, 0.0, 60.0);
        let mag_body = q_true.rotate_inv(Vec3::new(0.4, 0.0, 0.6));
        let accel = gravity_in_body(q_true);
        let dt = 0.01;
        // 收敛为指数逼近（时间常数 ~1/(kp·0.16)），需足够时长。
        for _ in 0..2500 {
            // 无陀螺：纯靠磁 + 加速度收敛。
            f.update_mag(Vec3::new(0.0, 0.0, 0.0), accel, mag_body, dt);
        }
        let e = f.euler();
        // 应接近 60°（允许 5° 误差）。
        let d = (e.yaw - 60.0).abs();
        assert!(d < 5.0, "yaw {} expected ~60", e.yaw);
        assert!(e.roll.abs() < 5.0 && e.pitch.abs() < 5.0, "r/p {} {}", e.roll, e.pitch);
    }
}
