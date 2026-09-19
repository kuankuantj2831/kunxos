//! 块设备抽象：只定义最小接口，不含任何调度、缓存或分区逻辑。

use crate::error::KosError;

/// 块设备。读写单位为整块；`buf`/`data` 长度必须等于 `block_size`。
///
/// 设计约束（Phase 1 Step 1）：
/// - 本 trait 只有 `block_size` / `block_count` / `read_block` /
///   `write_block` / `flush` 五个方法；
/// - 对象安全（`&dyn BlockDevice` / `&mut dyn BlockDevice` 可用）；
/// - 读写只做边界与长度校验，不解析内容。
pub trait BlockDevice {
    /// 块大小（字节）。
    fn block_size(&self) -> usize;
    /// 设备总块数。
    fn block_count(&self) -> u64;
    /// 读第 `lba` 块到 `buf`。
    fn read_block(&self, lba: u64, buf: &mut [u8]) -> Result<(), KosError>;
    /// 把 `data` 写入第 `lba` 块。
    fn write_block(&mut self, lba: u64, data: &[u8]) -> Result<(), KosError>;
    /// 把已写数据冲刷到持久介质。
    fn flush(&mut self) -> Result<(), KosError>;
}
