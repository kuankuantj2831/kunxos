//! 「星辰之光」控件：亮度滑块 / 星辰开关 / 星尘输入框 + 共享粒子系统。
//!
//! 模块划分：
//! - [`ParticleSystem`]：定长粒子池（≤500）+「列栈」沙堆网格 + 漏沙/蒸发/星点。
//! - [`BrightnessSlider`]：向沙漏容器注入 / 倾倒沙子，thumb 为发光星辰。
//! - [`StellarToggle`]：胶囊内沙子开关，可复用到任何「沙子控件」。
//! - [`NebulaInput`]：逐字浮现的星光笔迹输入框。

pub mod particle_system;
pub mod brightness_slider;
pub mod stellar_toggle;
pub mod nebula_input;

pub use particle_system::{MAX_COLS, MAX_PARTICLES, Particle, ParticleKind, ParticleSystem};
pub use brightness_slider::BrightnessSlider;
pub use stellar_toggle::{StellarToggle, ToggleAnimState};
pub use nebula_input::{Font, GlyphRaster, NebulaInput, TextCharAnim};