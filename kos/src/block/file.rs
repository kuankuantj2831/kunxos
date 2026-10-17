//! 宿主文件块设备（std only）：把一个镜像文件适配成 [`BlockDevice`]。

use crate::block::BlockDevice;
use crate::error::KosError;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

/// 以文件为后端的块设备。
pub struct FileBlockDevice {
    file: File,
    block_size: usize,
    block_count: u64,
}

impl FileBlockDevice {
    /// 打开（或创建）镜像文件。`create_blocks` 为 `Some(n)` 时创建
    /// `n` 块的新镜像。
    pub fn open(path: &Path, create_blocks: Option<u64>) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(create_blocks.is_some())
            .open(path)?;
        let block_size = crate::format::KOS_BLOCK_SIZE;
        let block_count = match create_blocks {
            Some(n) => {
                file.set_len(n * block_size as u64)?;
                n
            }
            None => file.metadata()?.len() / block_size as u64,
        };
        Ok(FileBlockDevice {
            file,
            block_size,
            block_count,
        })
    }
}

impl BlockDevice for FileBlockDevice {
    fn block_size(&self) -> usize {
        self.block_size
    }

    fn block_count(&self) -> u64 {
        self.block_count
    }

    fn read_block(&self, lba: u64, buf: &mut [u8]) -> Result<(), KosError> {
        check(self.block_size, self.block_count, lba, buf.len())?;
        let mut f = &self.file;
        f.seek(SeekFrom::Start(lba * self.block_size as u64))
            .map_err(io_err)?;
        f.read_exact(buf).map_err(io_err)?;
        Ok(())
    }

    fn write_block(&mut self, lba: u64, data: &[u8]) -> Result<(), KosError> {
        check(self.block_size, self.block_count, lba, data.len())?;
        let f = &mut self.file;
        f.seek(SeekFrom::Start(lba * self.block_size as u64))
            .map_err(io_err)?;
        f.write_all(data).map_err(io_err)?;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), KosError> {
        self.file.sync_all().map_err(io_err)
    }
}

fn check(bs: usize, bc: u64, lba: u64, len: usize) -> Result<(), KosError> {
    if lba >= bc {
        return Err(KosError::BlockOutOfRange {
            lba,
            block_count: bc,
        });
    }
    if len != bs {
        return Err(KosError::InvalidBlockLen {
            expected: bs,
            got: len,
        });
    }
    Ok(())
}

fn io_err(e: std::io::Error) -> KosError {
    KosError::Io(e.raw_os_error().unwrap_or(-1))
}
