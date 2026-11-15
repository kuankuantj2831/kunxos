//! Guest regression for VFS identity/permission enforcement (owned files on SFS).
//! A non-root child creates a 0600 file; a different non-root uid must be denied
//! read/write/unlink/rename; root and the owner must still succeed.
//!   python tools/test_audit_guest.py --qemu <qemu-system-riscv64> \
//!       --build-dir build/vfsperm-guest --source tests/vfs_perm_guest.rs \
//!       --success-marker "VFSPERM REGRESSION PASS"
#![no_std]
#![no_main]
use core::arch::{asm, global_asm};

unsafe fn call(n: usize, a: [usize; 6]) -> isize {
    let r: isize;
    asm!("ecall", in("a7") n, inlateout("a0") a[0] => r, in("a1") a[1], in("a2") a[2],
         in("a3") a[3], in("a4") a[4], in("a5") a[5], options(nostack));
    r
}
fn print(s: &str) { unsafe { call(64, [1, s.as_ptr() as usize, s.len(), 0, 0, 0]); } }

const CWD: usize = (-100isize) as usize;
const EACCES: isize = -13;
const PATH: &[u8] = b"/vfsperm-owned\0";
const PATH2: &[u8] = b"/vfsperm-renamed\0";

fn fail(label: &str) -> ! {
    print("VFSPERM GUEST FAIL: ");
    print(label);
    print("\n");
    unsafe { call(93, [1, 0, 0, 0, 0, 0]); }
    loop {}
}
fn check(ok: bool, label: &str) { if !ok { fail(label); } }

unsafe fn open(path: &[u8], flags: usize) -> isize { call(56, [CWD, path.as_ptr() as usize, flags, 0o644, 0, 0]) }
unsafe fn unlink(path: &[u8]) -> isize { call(35, [CWD, path.as_ptr() as usize, 0, 0, 0, 0]) }
unsafe fn chmod(path: &[u8], mode: usize) -> isize { call(53, [CWD, path.as_ptr() as usize, mode, 0, 0, 0]) }

/// Run `body` in a forked child; child exit code 0 = pass, nonzero = failing step.
unsafe fn in_child(uid: usize, body: unsafe fn() -> usize) -> usize {
    let pid = call(220, [17, 0, 0, 0, 0, 0]); // clone(SIGCHLD) == fork
    check(pid >= 0, "fork");
    if pid == 0 {
        if uid != 0 && call(146, [uid, 0, 0, 0, 0, 0]) != 0 { call(93, [100, 0, 0, 0, 0, 0]); }
        let code = body();
        call(93, [code, 0, 0, 0, 0, 0]);
        loop {}
    }
    let mut status: i32 = -1;
    check(call(260, [pid as usize, &mut status as *mut i32 as usize, 0, 0, 0, 0]) == pid, "wait4");
    ((status >> 8) & 0xff) as usize
}

unsafe fn owner_creates() -> usize {
    let fd = open(PATH, 0x242); // O_RDWR|O_CREAT|O_TRUNC
    if fd < 0 { return 1; }
    if call(64, [fd as usize, b"secret".as_ptr() as usize, 6, 0, 0, 0]) != 6 { return 2; }
    call(57, [fd as usize, 0, 0, 0, 0, 0]);
    if chmod(PATH, 0o600) != 0 { return 3; }
    // owner keeps access
    let fd = open(PATH, 2);
    if fd < 0 { return 4; }
    call(57, [fd as usize, 0, 0, 0, 0, 0]);
    0
}

unsafe fn stranger_denied() -> usize {
    if open(PATH, 0) != EACCES { return 1; }          // read
    if open(PATH, 1) != EACCES { return 2; }          // write
    if open(PATH, 2) != EACCES { return 3; }          // read-write
    if unlink(PATH) != EACCES { return 4; }           // unlink
    let r = call(38, [CWD, PATH.as_ptr() as usize, CWD, PATH2.as_ptr() as usize, 0, 0]); // renameat
    if r != EACCES { return 5; }
    if chmod(PATH, 0o777) == 0 { return 6; }          // must not chmod someone else's file
    0
}

unsafe fn owner_unlinks() -> usize {
    if unlink(PATH) != 0 { return 1; }
    if open(PATH, 0) >= 0 { return 2; }
    0
}

#[no_mangle]
pub extern "C" fn k_main() -> usize {
    print("VFSPERM REGRESSION START\n");
    unsafe {
        unlink(PATH); unlink(PATH2);
        check(in_child(10001, owner_creates) == 0, "owner creates 0600 file and keeps access");
        print("VFSPERM owner PASS\n");

        let sc = in_child(10002, stranger_denied);
        if sc != 0 {
            print("VFSPERM stranger step failed: ");
            let d = [b'0' + (sc % 10) as u8, b'\n'];
            call(64, [1, d.as_ptr() as usize, 2, 0, 0, 0]);
        }
        check(sc == 0, "other uid denied read/write/unlink/rename/chmod");
        print("VFSPERM stranger PASS\n");

        // root is unaffected by ownership
        let fd = open(PATH, 2);
        check(fd >= 0, "root can open another user's 0600 file");
        let mut buf = [0u8; 8];
        check(call(63, [fd as usize, buf.as_mut_ptr() as usize, 8, 0, 0, 0]) == 6 && &buf[..6] == b"secret", "root reads intact content");
        call(57, [fd as usize, 0, 0, 0, 0, 0]);
        print("VFSPERM root PASS\n");

        // file survived the denied attempts; owner can still delete it
        check(in_child(10001, owner_unlinks) == 0, "owner can unlink own file after denied attempts");
        print("VFSPERM cleanup PASS\n");
    }
    print("VFSPERM REGRESSION PASS\n");
    0
}

global_asm!(".section .text._start, \"ax\"", ".global _start", "_start:", "call k_main", "li a7,93", "ecall", "j .");
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { print("VFSPERM GUEST FAIL: panic\n"); loop {} }
