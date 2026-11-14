//! 应用侧平台实现：把同步栈的 Fs / Net trait 接到 KunxOS 裸 syscall。
//!
//! 由日历 / 通讯录应用经 `#[path = "../../../shared/sync/app_platform.rs"]`
//! 引入（依赖 `crate::kit`，即 shared/app_kit.rs）。

use crate::kit::*;
use crate::sync::fs::Fs;
use crate::sync::http::Net;

// —— 文件系统常量（与内核 linux_compat 一致） ——
const O_CREAT: usize = 0x40;
const O_TRUNC: usize = 0x200;
const O_WRONLY: usize = 1;
const SYS_LSEEK: usize = 62;
const SYS_UNLINKAT: usize = 35;

/// SFS 整文件读写实现（open/read/write/lseek/close/unlinkat）。
pub struct SysFs;

impl Fs for SysFs {
    fn read_file(&mut self, path: &[u8], buf: &mut [u8]) -> isize {
        let mut p = [0u8; 128];
        sync_copy(&mut p, path);
        unsafe {
            let fd = syscall3(SYS_OPENAT, AT_FDCWD, p.as_ptr() as usize, O_RDONLY);
            if (fd as isize) < 0 {
                return -1;
            }
            let n = syscall3(SYS_READ, fd, buf.as_mut_ptr() as usize, buf.len());
            syscall1(SYS_CLOSE, fd);
            n as isize
        }
    }

    fn write_file(&mut self, path: &[u8], data: &[u8]) -> bool {
        let mut p = [0u8; 128];
        sync_copy(&mut p, path);
        unsafe {
            let fd = syscall3(SYS_OPENAT, AT_FDCWD, p.as_ptr() as usize, O_CREAT | O_TRUNC | O_WRONLY);
            if (fd as isize) < 0 {
                return false;
            }
            let n = syscall3(SYS_WRITE, fd, data.as_ptr() as usize, data.len());
            syscall1(SYS_CLOSE, fd);
            n as isize == data.len() as isize
        }
    }

    fn write_file_at(&mut self, path: &[u8], off: u32, data: &[u8]) -> bool {
        let mut p = [0u8; 128];
        sync_copy(&mut p, path);
        unsafe {
            let fd = syscall3(SYS_OPENAT, AT_FDCWD, p.as_ptr() as usize, O_CREAT | O_WRONLY);
            if (fd as isize) < 0 {
                return false;
            }
            if syscall3(SYS_LSEEK, fd, off as usize, 0) as isize != off as isize {
                syscall1(SYS_CLOSE, fd);
                return false;
            }
            let n = syscall3(SYS_WRITE, fd, data.as_ptr() as usize, data.len());
            syscall1(SYS_CLOSE, fd);
            n as isize == data.len() as isize
        }
    }

    fn delete_file(&mut self, path: &[u8]) -> bool {
        let mut p = [0u8; 128];
        sync_copy(&mut p, path);
        unsafe { syscall3(SYS_UNLINKAT, AT_FDCWD, p.as_ptr() as usize, 0) as isize == 0 }
    }

    fn file_exists(&mut self, path: &[u8]) -> bool {
        let mut p = [0u8; 128];
        sync_copy(&mut p, path);
        unsafe {
            let fd = syscall3(SYS_OPENAT, AT_FDCWD, p.as_ptr() as usize, O_RDONLY);
            if (fd as isize) < 0 {
                return false;
            }
            syscall1(SYS_CLOSE, fd);
            true
        }
    }
}

/// 内核 TCP 栈传输实现（阻塞式；与 cloud 应用同一套 socket 系统调用）。
pub struct SysNet;

impl Net for SysNet {
    fn connect(&mut self, host: &[u8], port: u16) -> isize {
        let ip = match ip_parse(host) {
            Some(ip) => ip,
            None => unsafe { dns_resolve(host) },
        };
        if ip == 0 {
            return -1;
        }
        unsafe {
            let fd = sock_open();
            if fd < 0 {
                return -1;
            }
            if sock_connect(fd, ip, port) < 0 {
                sock_close(fd);
                return -1;
            }
            fd
        }
    }

    fn send(&mut self, h: isize, data: &[u8]) -> isize {
        unsafe { sock_send(h, data) }
    }

    fn recv(&mut self, h: isize, buf: &mut [u8]) -> isize {
        unsafe { sock_recv(h, buf) }
    }

    fn close(&mut self, h: isize) {
        unsafe { sock_close(h); }
    }
}

fn sync_copy(dst: &mut [u8], src: &[u8]) -> usize {
    let n = src.len().min(dst.len() - 1);
    dst[..n].copy_from_slice(&src[..n]);
    dst[n] = 0;
    n
}
