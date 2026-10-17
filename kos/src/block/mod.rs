//! 块设备抽象与内存实现。
//!
//! 文件 `trait.rs` 的模块名 `trait` 是 Rust 关键字，故以 `#[path]`
//! 映射为 `device` 子模块。

#[path = "trait.rs"]
pub mod device;

pub use device::BlockDevice;

#[cfg(feature = "std")]
pub mod file;

use crate::error::KosError;
use alloc::vec;

/// 内存块设备：`Vec<u8>` 后备，**仅测试用**（宿主单测、几何演练）。
///
/// 行为：零初始化；读写整块；越界与长度不符返回 `Err`；
/// `flush` 为空操作（内存介质无持久化语义）。
pub struct MemBlockDevice {
    storage: vec::Vec<u8>,
    block_size: usize,
    block_count: u64,
}

impl MemBlockDevice {
    /// 创建 `block_count` × `block_size` 的零填充设备。
    pub fn new(block_count: u64, block_size: usize) -> Self {
        assert!(block_size > 0, "块大小必须非零");
        let total = (block_count as usize)
            .checked_mul(block_size)
            .expect("MemBlockDevice 容量溢出 usize");
        MemBlockDevice {
            storage: vec![0u8; total],
            block_size,
            block_count,
        }
    }
}

impl BlockDevice for MemBlockDevice {
    fn block_size(&self) -> usize {
        self.block_size
    }

    fn block_count(&self) -> u64 {
        self.block_count
    }

    fn read_block(&self, lba: u64, buf: &mut [u8]) -> Result<(), KosError> {
        if lba >= self.block_count {
            return Err(KosError::BlockOutOfRange {
                lba,
                block_count: self.block_count,
            });
        }
        if buf.len() != self.block_size {
            return Err(KosError::InvalidBlockLen {
                expected: self.block_size,
                got: buf.len(),
            });
        }
        let off = lba as usize * self.block_size;
        buf.copy_from_slice(&self.storage[off..off + self.block_size]);
        Ok(())
    }

    fn write_block(&mut self, lba: u64, data: &[u8]) -> Result<(), KosError> {
        if lba >= self.block_count {
            return Err(KosError::BlockOutOfRange {
                lba,
                block_count: self.block_count,
            });
        }
        if data.len() != self.block_size {
            return Err(KosError::InvalidBlockLen {
                expected: self.block_size,
                got: data.len(),
            });
        }
        let off = lba as usize * self.block_size;
        self.storage[off..off + self.block_size].copy_from_slice(data);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), KosError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_read_roundtrip() {
        let mut dev = MemBlockDevice::new(4, 512);
        assert_eq!(dev.block_size(), 512);
        assert_eq!(dev.block_count(), 4);
        let block: Vec<u8> = (0..512).map(|i| (i * 7) as u8).collect();
        dev.write_block(2, &block).unwrap();
        dev.flush().unwrap();
        let mut buf = [0u8; 512];
        dev.read_block(2, &mut buf).unwrap();
        assert_eq!(&buf[..], &block[..]);
        // 未写的块保持零
        dev.read_block(0, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[test]
    fn rejects_out_of_range_and_bad_length() {
        let mut dev = MemBlockDevice::new(4, 512);
        assert!(matches!(
            dev.write_block(4, &[0u8; 512]).unwrap_err(),
            KosError::BlockOutOfRange { lba: 4, .. }
        ));
        assert!(matches!(
            dev.read_block(0, &mut [0u8; 511]).unwrap_err(),
            KosError::InvalidBlockLen { expected: 512, got: 511 }
        ));
        assert!(matches!(
            dev.write_block(0, &[0u8; 513]).unwrap_err(),
            KosError::InvalidBlockLen { expected: 512, got: 513 }
        ));
    }

    #[test]
    fn usable_through_trait_object() {
        fn write_first_block(dev: &mut dyn BlockDevice, data: &[u8]) {
            dev.write_block(0, data).unwrap();
        }
        let mut dev = MemBlockDevice::new(2, 4096);
        write_first_block(&mut dev, &[0xABu8; 4096]);
        let mut buf = [0u8; 4096];
        dev.read_block(0, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0xAB));
    }
}
