//! DDR 内存初始化。
//!
//! QEMU virt：RAM 已由 QEMU 初始化，无需配置，直接返回。
//! Raspberry Pi 4：DDR 已由 VideoCore GPU 固件（bootcode/EEPROM）初始化，
//! bootloader 无需重复配置，此处为骨架（编译通过即可）。

/// 初始化 DDR 内存。
///
/// QEMU 与 Pi 4 均由外部完成 DDR 初始化，本函数为空实现；
/// 保留接口以对应手机 Bootloader 流程中 SPL 的 DDR 训练步骤。
pub fn init() {
    // QEMU: RAM 已就绪，跳过。
    // Pi 4:  GPU 固件已初始化 DDR，跳过。
}
