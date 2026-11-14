//! 文件系统抽象：同步栈与平台解耦。
//!
//! 应用侧实现基于 KunxOS 裸 syscall（open/read/write/lseek/close/unlinkat，
//! 见 apps/*/src/main.rs 中 notes 应用的既有模式）；宿主测试侧实现基于
//! `std::fs`。两种实现都遵循"整文件"语义，存储层（store.rs）只依赖本 trait。

/// 文件系统操作的最小抽象（整文件读写）。
pub trait Fs {
    /// 读取整个文件到 buf。返回读取字节数；文件不存在 / 出错返回 -1。
    fn read_file(&mut self, path: &[u8], buf: &mut [u8]) -> isize;

    /// 覆盖写入整个文件（创建或截断）。成功返回 true。
    fn write_file(&mut self, path: &[u8], data: &[u8]) -> bool;

    /// 按偏移写入一段，保留范围以外的既有内容；不存在时创建。
    /// 重建文件的调用方必须先用 write_file(path, &[]) 截断一次。
    fn write_file_at(&mut self, path: &[u8], off: u32, data: &[u8]) -> bool;

    /// 删除文件。成功（或不存在）返回 true。
    fn delete_file(&mut self, path: &[u8]) -> bool;

    /// 文件是否存在。
    fn file_exists(&mut self, path: &[u8]) -> bool;
}
