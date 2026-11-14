//! 宿主平台实现：std 文件系统与 TCP 传输（测试用）。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;

use crate::sync::fs::Fs;
use crate::sync::http::Net;

/// std::fs 整文件实现（与 KunxOS SFS 应用实现的语义一致）。
pub struct HostFs {
    pub root: std::path::PathBuf,
}

impl HostFs {
    pub fn new(root: std::path::PathBuf) -> Self {
        std::fs::create_dir_all(&root).ok();
        HostFs { root }
    }

    fn path(&self, p: &[u8]) -> std::path::PathBuf {
        let s = String::from_utf8_lossy(p);
        // 去掉开头的 /
        let s = s.trim_start_matches('/');
        self.root.join(s)
    }
}

impl Fs for HostFs {
    fn read_file(&mut self, path: &[u8], buf: &mut [u8]) -> isize {
        match std::fs::read(self.path(path)) {
            Ok(data) => {
                let n = data.len().min(buf.len());
                buf[..n].copy_from_slice(&data[..n]);
                n as isize
            }
            Err(_) => -1,
        }
    }

    fn write_file(&mut self, path: &[u8], data: &[u8]) -> bool {
        let p = self.path(path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(p, data).is_ok()
    }

    fn write_file_at(&mut self, path: &[u8], off: u32, data: &[u8]) -> bool {
        let p = self.path(path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        use std::io::{Seek, SeekFrom};
        match std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&p) {
            Ok(mut f) => {
                if f.seek(SeekFrom::Start(off as u64)).is_err() {
                    return false;
                }
                f.write_all(data).is_ok()
            }
            Err(_) => false,
        }
    }

    fn delete_file(&mut self, path: &[u8]) -> bool {
        std::fs::remove_file(self.path(path)).is_ok()
    }

    fn file_exists(&mut self, path: &[u8]) -> bool {
        self.path(path).exists()
    }
}

/// std::net::TcpStream 传输。
pub struct HostNet;

impl Net for HostNet {
    fn connect(&mut self, host: &[u8], port: u16) -> isize {
        let host = String::from_utf8_lossy(host).to_string();
        match TcpStream::connect((host.as_str(), port)) {
            Ok(s) => {
                let h = Box::into_raw(Box::new(s)) as isize;
                h
            }
            Err(_) => -1,
        }
    }

    fn send(&mut self, h: isize, data: &[u8]) -> isize {
        let s = unsafe { &mut *(h as *mut TcpStream) };
        s.write_all(data).map(|_| data.len() as isize).unwrap_or(-1)
    }

    fn recv(&mut self, h: isize, buf: &mut [u8]) -> isize {
        let s = unsafe { &mut *(h as *mut TcpStream) };
        s.read(buf).map(|n| n as isize).unwrap_or(-1)
    }

    fn close(&mut self, h: isize) {
        unsafe { drop(Box::from_raw(h as *mut TcpStream)); }
    }
}

/// 内存文件系统（测试隔离）。
pub struct MemFs {
    pub files: HashMap<Vec<u8>, Vec<u8>>,
}

impl MemFs {
    pub fn new() -> Self {
        MemFs { files: HashMap::new() }
    }
}

impl Fs for MemFs {
    fn read_file(&mut self, path: &[u8], buf: &mut [u8]) -> isize {
        match self.files.get(path) {
            Some(d) => {
                let n = d.len().min(buf.len());
                buf[..n].copy_from_slice(&d[..n]);
                n as isize
            }
            None => -1,
        }
    }

    fn write_file(&mut self, path: &[u8], data: &[u8]) -> bool {
        self.files.insert(path.to_vec(), data.to_vec());
        true
    }

    fn write_file_at(&mut self, path: &[u8], off: u32, data: &[u8]) -> bool {
        let e = self.files.entry(path.to_vec()).or_default();
        let end = off as usize + data.len();
        if e.len() < end {
            e.resize(end, 0);
        }
        e[off as usize..end].copy_from_slice(data);
        true
    }

    fn delete_file(&mut self, path: &[u8]) -> bool {
        self.files.remove(path);
        true
    }

    fn file_exists(&mut self, path: &[u8]) -> bool {
        self.files.contains_key(path)
    }
}
