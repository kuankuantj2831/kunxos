//! 运动识别：计步器（加速度阈值 + 峰值检测）与抬腕亮屏检测。
//!
//! - [`StepDetector`]：对加速度模长做阈值-峰值状态机。简化方案
//!   （任务约定「可先简化：加速度阈值检测」）：`|a|` 上升穿过
//!   `high`（默认 1.15g）进入 Rising，回落到 `high - delta`（默认 1.05g）
//!   且距上一步 ≥ `min_interval`（默认 300ms）计一步。可加
//!   300ms~2s 步频窗口过滤伪步。
//! - [`RaiseDetector`]：抬腕亮屏。设备由「平放（z 向上 > 0.6g）」转为
//!   「竖持（z 降到 0.35g 以下且 x/y 出现 > 0.7g 分量）」且期间陀螺
//!   角速度超过 `gyro_thresh`（手腕动作）→ 触发一次抬腕事件。
//!
//! 两者均为纯状态机，无堆、无浮点库依赖（仅 `crate::math::sqrt`）。

use crate::fusion::Vec3;
use crate::math;

// ==================== 计步器 ====================

/// 计步器状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StepState {
    Idle,
    Rising,
}

/// 加速度阈值计步器。
#[derive(Clone, Copy, Debug)]
pub struct StepDetector {
    /// 累计步数。
    pub count: u32,
    /// 上升触发阈值（g）。
    pub high: f32,
    /// 回落判定：`high - delta`（g）。
    pub delta: f32,
    /// 最小步间隔（秒）。
    pub min_interval: f32,
    /// 单步最大时长（秒，超过则放弃）。
    pub max_step_time: f32,
    /// 累计仿真时间（秒）。
    t: f32,
    /// 峰值。
    peak: f32,
    /// 本次步开始时间。
    rise_t: f32,
    /// 上一步完成时间。
    last_step_t: f32,
    state: StepState,
}

impl Default for StepDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl StepDetector {
    pub fn new() -> Self {
        StepDetector {
            count: 0,
            high: 1.15,
            delta: 0.10,
            min_interval: 0.30,
            max_step_time: 0.80,
            t: 0.0,
            peak: 0.0,
            rise_t: 0.0,
            last_step_t: -1.0,
            state: StepState::Idle,
        }
    }

    /// 重置计步器。
    pub fn reset(&mut self) {
        self.count = 0;
        self.t = 0.0;
        self.peak = 0.0;
        self.last_step_t = -1.0;
        self.state = StepState::Idle;
    }

    /// 喂入一个加速度样本（g），返回是否完成一步。
    pub fn update(&mut self, accel: Vec3, dt: f32) -> bool {
        self.t += dt;
        let mag = math::sqrt(accel.x * accel.x + accel.y * accel.y + accel.z * accel.z);
        let low = self.high - self.delta;
        let mut step = false;
        match self.state {
            StepState::Idle => {
                // 上升穿过 high 且距上一步足够久 → 进入 Rising。
                if mag >= self.high
                    && (self.last_step_t < 0.0 || self.t - self.last_step_t >= self.min_interval)
                {
                    self.state = StepState::Rising;
                    self.peak = mag;
                    self.rise_t = self.t;
                }
            }
            StepState::Rising => {
                if mag > self.peak {
                    self.peak = mag;
                }
                if mag <= low {
                    // 回落到 low 以下 → 一步完成。
                    if self.t - self.last_step_t >= self.min_interval || self.last_step_t < 0.0 {
                        self.count += 1;
                        step = true;
                    }
                    self.last_step_t = self.t;
                    self.state = StepState::Idle;
                } else if self.t - self.rise_t > self.max_step_time {
                    // 超时放弃（可能是慢速抬放而非步伐）。
                    self.state = StepState::Idle;
                }
            }
        }
        step
    }
}

// ==================== 抬腕检测 ====================

/// 抬腕检测器状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RaiseState {
    /// 平放（屏幕朝上）或未抬手。
    Flat,
    /// 正在抬手（过渡中）。
    Raising,
    /// 已抬起（等待放回）。
    Raised,
}

/// 抬腕亮屏检测器。
///
/// 事件条件（全部满足）：
/// 1. 先前处于平放（accel.z > 0.6g）
/// 2. 过渡期间陀螺模长超过 `gyro_thresh`（手腕动作，默认 60°/s）
/// 3. 抬起后 accel.z < 0.35g 且 x 或 y 出现 > 0.7g 分量（竖持/面向用户）
/// 4. 距上次事件 ≥ `cooldown`（默认 2s）
#[derive(Clone, Copy, Debug)]
pub struct RaiseDetector {
    /// 抬腕事件计数。
    pub count: u32,
    /// 平放判定阈值（g）。
    pub flat_z: f32,
    /// 抬起后 z 阈值（g）。
    pub raised_z: f32,
    /// 侧向分量阈值（g）。
    pub side_g: f32,
    /// 陀螺模长阈值（°/s）。
    pub gyro_thresh: f32,
    /// 事件冷却（秒）。
    pub cooldown: f32,
    /// 过渡超时（秒）。
    pub timeout: f32,
    t: f32,
    raise_t: f32,
            /// 上次事件时刻（冷却基准：raise_t 是本次 Raising 起点，
            /// 与 1.5s 超时同源 —— 1.5 < cooldown 2.0，以它为基准冷却条件恒假，
            /// 抬腕一生只触发一次）。
            last_event_t: f32,
    state: RaiseState,
}

impl Default for RaiseDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl RaiseDetector {
    pub fn new() -> Self {
        RaiseDetector {
            count: 0,
            flat_z: 0.6,
            raised_z: 0.35,
            side_g: 0.7,
            gyro_thresh: 60.0,
            cooldown: 2.0,
            timeout: 1.5,
            t: 0.0,
            raise_t: 0.0,
            last_event_t: -1.0e9,
            state: RaiseState::Flat,
        }
    }

    /// 重置计数与状态机（保留阈值标定）。sensord 的 RESET 命令走此路径。
    pub fn reset(&mut self) {
        self.count = 0;
        self.t = 0.0;
        self.raise_t = 0.0;
        self.last_event_t = -1.0e9;
        self.state = RaiseState::Flat;
    }

    /// 喂入加速度（g）与陀螺（°/s），返回本次是否触发抬腕事件。
    pub fn update(&mut self, accel: Vec3, gyro: Vec3, dt: f32) -> bool {
        self.t += dt;
        let gmag = math::sqrt(gyro.x * gyro.x + gyro.y * gyro.y + gyro.z * gyro.z);
        let side = if accel.x < 0.0 { -accel.x } else { accel.x }
            .max(if accel.y < 0.0 { -accel.y } else { accel.y });
        let mut raised = false;
        match self.state {
            RaiseState::Flat => {
                if accel.z <= self.flat_z && gmag >= self.gyro_thresh {
                    self.state = RaiseState::Raising;
                    self.raise_t = self.t;
                }
            }
            RaiseState::Raising => {
                if accel.z < self.raised_z && side > self.side_g {
                    // 冷却相对「上次事件时刻」：此前相对 raise_t（≤1.5s 前，
                    // 而冷却 2.0s）恒假 —— 首次之后永不计数。
                    if self.count == 0 || self.t - self.last_event_t >= self.cooldown {
                        self.count += 1;
                        self.last_event_t = self.t;
                        raised = true;
                    }
                    self.state = RaiseState::Raised;
                } else if accel.z > self.flat_z {
                    // 没抬起来就放回去了。
                    self.state = RaiseState::Flat;
                } else if self.t - self.raise_t > self.timeout {
                    // 过渡超时（慢速移动）。
                    self.state = RaiseState::Flat;
                }
            }
            RaiseState::Raised => {
                if accel.z > self.flat_z {
                    // 放回平放。
                    self.state = RaiseState::Flat;
                }
            }
        }
        raised
    }

    /// 当前是否处于抬起状态（供电源/显示策略参考）。
    pub fn is_raised(&self) -> bool {
        self.state == RaiseState::Raised || self.state == RaiseState::Raising
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 模拟行走：z 向 0.4g 冲击。
    fn walk_step() -> Vec3 {
        Vec3::new(0.0, 0.0, 1.4)
    }

    #[test]
    fn test_step_detector() {
        let mut d = StepDetector::new();
        let dt = 0.01;
        let rest = Vec3::new(0.0, 0.0, 1.0);
        // 静止 1s。
        for _ in 0..100 {
            d.update(rest, dt);
        }
        assert_eq!(d.count, 0);
        // 一步：上升 → 回落。
        for _ in 0..10 {
            d.update(walk_step(), dt);
        }
        for _ in 0..20 {
            d.update(rest, dt);
        }
        assert_eq!(d.count, 1);
        // 立刻再一步会被 min_interval 拦截。
        for _ in 0..10 {
            d.update(walk_step(), dt);
        }
        for _ in 0..20 {
            d.update(rest, dt);
        }
        assert_eq!(d.count, 1);
    }

    #[test]
    fn test_step_detector_walking() {
        let mut d = StepDetector::new();
        let dt = 0.01;
        // 每 0.6s 一步，走 10 步。
        for step in 0..10u32 {
            for i in 0..60 {
                let k = if i < 15 {
                    (i as f32 / 15.0 * core::f32::consts::PI).sin()
                } else {
                    0.0
                };
                let a = Vec3::new(0.0, 0.15 * k, 1.0 + 0.4 * k);
                d.update(a, dt);
            }
            let _ = step;
        }
        assert_eq!(d.count, 10);
    }

    #[test]
    fn test_raise_detector() {
        let mut d = RaiseDetector::new();
        let dt = 0.01;
        let flat = Vec3::new(0.0, 0.0, 1.0);
        for _ in 0..100 {
            d.update(flat, Vec3::new(0.0, 0.0, 0.0), dt);
        }
        assert_eq!(d.count, 0);
        // 抬手：z→0, y→1，陀螺 x 90°/s。
        for i in 0..50 {
            let k = (i as f32 / 50.0) * core::f32::consts::PI / 2.0;
            let a = Vec3::new(0.0, k.sin(), k.cos());
            let g = Vec3::new(90.0, 0.0, 0.0);
            if d.update(a, g, dt) {
                break;
            }
        }
        assert_eq!(d.count, 1);
        // 放回。
        for _ in 0..100 {
            d.update(flat, Vec3::new(0.0, 0.0, 0.0), dt);
        }
        // 冷却期内再抬手不计数。
        for i in 0..50 {
            let k = (i as f32 / 50.0) * core::f32::consts::PI / 2.0;
            let a = Vec3::new(0.0, k.sin(), k.cos());
            if d.update(a, Vec3::new(90.0, 0.0, 0.0), dt) {
                break;
            }
        }
        assert_eq!(d.count, 1);
    }
}
