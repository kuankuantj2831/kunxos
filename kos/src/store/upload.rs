//! 断点续传上传会话（E4）。
//!
//! 场景：向 KOS 写大文件（OTA 包、媒体、备份），传输中断电——重启后
//! **从缺的块继续**，已传的不重传。
//!
//! 结构（`path` 为最终路径）：
//! - `{path}.kosup.part`：分块暂存可变记录，begin 时预扩展到 total_len；
//! - `{path}.kosup.meta`：进度 sidecar（小对象）：已收块位图。
//!
//! 崩溃一致性：每次 push 先写 part 数据块（CoW + 头块原子切换），**后**
//! 重写 meta 位图——断电后 meta 只可能欠记（块已到但未记账），重传该块
//! 幂等无害，绝无超记（未到却记账）。finish 时 rename 一举发布（原子）。

use super::KosStore;
use crate::error::KosError;
use alloc::vec::Vec;

/// 进度 sidecar magic。
const META_MAGIC: &[u8; 4] = b"KUP1";
/// part 后缀。
pub const PART_EXT: &str = ".kosup.part";
/// meta 后缀。
pub const META_EXT: &str = ".kosup.meta";

fn part_path(path: &str) -> alloc::string::String {
    alloc::format!("{path}{PART_EXT}")
}

fn meta_path(path: &str) -> alloc::string::String {
    alloc::format!("{path}{META_EXT}")
}

impl<B: crate::block::BlockDevice> KosStore<B> {
    /// 开启上传会话：part 预扩展到 `total_len`，meta 清零。已存在会话
    /// 则保留进度（幂等续传入口）。
    pub fn upload_begin(&mut self, path: &str, total_len: u64, uid: u32, caps: u32) -> Result<(), KosError> {
        let part = part_path(path);
        let meta = meta_path(path);
        let chunks = total_len.div_ceil(super::DATA_CHUNK_SIZE as u64) as u32;
        // 幂等：已有会话（meta 可解析）→ 直接返回，进度保留。
        if let Ok(Some(_)) = self.lookup_entry_pub(&meta) {
            if let Ok(data) = self.open_path(&meta, uid) {
                if data.len() >= 12 && &data[..4] == META_MAGIC {
                    let old_total = u64::from_le_bytes(data[4..12].try_into().unwrap());
                    if old_total == total_len {
                        return Ok(());
                    }
                }
            }
        }
        self.write_mutable(&part, &[], uid, caps)?;
        self.truncate(&part, total_len, uid)?;
        let bitmap = alloc::vec![0u8; bitmap_bytes(chunks)];
        self.write_meta(&meta, total_len, &bitmap, uid)
    }

    /// 推送一块数据（任意偏移；建议按 chunk 对齐以最大化记账）。
    /// 返回 (已收块数, 总块数)。
    pub fn upload_push(
        &mut self,
        path: &str,
        offset: u64,
        data: &[u8],
        uid: u32,
    ) -> Result<(u32, u32), KosError> {
        let part = part_path(path);
        let meta = meta_path(path);
        // 1) 数据先落（CoW + 原子提交）。
        self.write_range(&part, offset, data, uid)?;
        // 2) 更新位图（后写：断电只欠记不超记）。
        let (total_len, mut bitmap) = self.read_meta(&meta, uid)?;
        let total = total_len.div_ceil(super::DATA_CHUNK_SIZE as u64) as u32;
        let lo = offset as usize / super::DATA_CHUNK_SIZE;
        let hi = ((offset as usize) + data.len()).div_ceil(super::DATA_CHUNK_SIZE);
        for c in lo..hi.min(total as usize) {
            let cstart = c * super::DATA_CHUNK_SIZE;
            let cend = core::cmp::min(cstart + super::DATA_CHUNK_SIZE, total_len as usize);
            // 块被完整覆盖才记账（半块重传幂等）。
            if offset as usize <= cstart && offset as usize + data.len() >= cend {
                bitmap[c / 8] |= 1 << (c % 8);
            }
        }
        self.write_meta(&meta, total_len, &bitmap, uid)?;
        let received = bitmap.iter().map(|b| b.count_ones()).sum::<u32>();
        Ok((received, total))
    }

    /// 会话状态：(total_len, 已收块, 连续前缀块)。
    pub fn upload_status(&mut self, path: &str, uid: u32) -> Result<(u64, u32, u32), KosError> {
        let (total_len, bitmap) = self.read_meta(&meta_path(path), uid)?;
        let total = total_len.div_ceil(super::DATA_CHUNK_SIZE as u64) as u32;
        let received = bitmap.iter().map(|b| b.count_ones()).sum::<u32>();
        let mut contiguous = 0u32;
        while contiguous < total
            && bitmap[(contiguous / 8) as usize] & (1 << (contiguous % 8)) != 0
        {
            contiguous += 1;
        }
        Ok((total_len, received, contiguous))
    }

    /// 完成上传：全部块到齐后 rename 原子发布，清理会话对象。
    pub fn upload_finish(&mut self, path: &str, uid: u32) -> Result<(), KosError> {
        let (total_len, received, _) = self.upload_status(path, uid)?;
        let total = total_len.div_ceil(super::DATA_CHUNK_SIZE as u64) as u32;
        if received < total {
            return Err(KosError::UploadIncomplete {
                received,
                total,
            });
        }
        self.rename(&part_path(path), path, uid)?;
        self.unlink(&meta_path(path), uid)
    }

    // ---------- meta 编解码 ----------

    fn read_meta(&mut self, meta: &str, uid: u32) -> Result<(u64, Vec<u8>), KosError> {
        let data = self.open_path(meta, uid)?;
        if data.len() < 12 || &data[..4] != META_MAGIC {
            return Err(KosError::ExtentTableCorrupt);
        }
        let total = u64::from_le_bytes(data[4..12].try_into().unwrap());
        Ok((total, data[12..].to_vec()))
    }

    fn write_meta(
        &mut self,
        meta: &str,
        total_len: u64,
        bitmap: &[u8],
        uid: u32,
    ) -> Result<(), KosError> {
        let mut content = Vec::with_capacity(12 + bitmap.len());
        content.extend_from_slice(META_MAGIC);
        content.extend_from_slice(&total_len.to_le_bytes());
        content.extend_from_slice(bitmap);
        self.write_mutable(meta, &content, uid, super::perm::DEFAULT)
            .map(|_| ())
    }
}

fn bitmap_bytes(chunks: u32) -> usize {
    chunks.div_ceil(8) as usize
}
