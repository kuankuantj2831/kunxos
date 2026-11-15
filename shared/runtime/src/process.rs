use crate::syscall;
use core::fmt::Write;

/// Write a complete buffer, including short writes and interrupted syscalls.
pub fn write_all(fd: usize, mut data: &[u8]) -> Result<(), syscall::Errno> {
    while !data.is_empty() {
        match syscall::result(unsafe { syscall::syscall3(64, fd, data.as_ptr() as usize, data.len()) }) {
            Ok(0) => return Err(syscall::Errno(5)),
            Ok(n) if n <= data.len() => data = &data[n..],
            Ok(_) => return Err(syscall::Errno(5)),
            Err(syscall::Errno(4)) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

pub fn exit(status: usize) -> ! {
    unsafe { syscall::syscall1(94, status); syscall::syscall1(93, status); }
    loop { core::hint::spin_loop(); }
}

/// Fork while serializing the runtime heap, so its lock is released in both
/// process copies. Multithreaded callers must exec/exit in the child before
/// using other application-owned locks, as with normal fork semantics.
pub unsafe fn fork() -> usize {
    crate::heap::fork_with_heap_lock()
}

pub fn report_panic(label: &str, info: &core::panic::PanicInfo<'_>) {
    struct Buffer { bytes: [u8; 384], len: usize }
    impl Write for Buffer {
        fn write_str(&mut self, text: &str) -> core::fmt::Result {
            let n = text.len().min(self.bytes.len() - self.len);
            self.bytes[self.len..self.len + n].copy_from_slice(&text.as_bytes()[..n]);
            self.len += n; Ok(())
        }
    }
    let mut b = Buffer { bytes: [0; 384], len: 0 };
    let _ = writeln!(b, "PANIC: {label}");
    let _ = writeln!(b, "{info}");
    let _ = write_all(2, &b.bytes[..b.len]);
}
