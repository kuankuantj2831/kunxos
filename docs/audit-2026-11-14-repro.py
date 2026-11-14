"""Reproduce audit findings using current source, without editing implementation.

Run: python docs/audit-2026-11-14-repro.py
The kernel functions run with host stubs; this is not a guest integration test.
Tests assert the observed bugs, so passing means the finding was reproduced.
"""
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / 'build/audit-2026-11-14/repro'
OUT.mkdir(parents=True, exist_ok=True)


def function(source, name):
    start = source.index('fn ' + name + '(')
    if source[max(0, start-7):start].endswith('unsafe '):
        start -= 7
    body = source.index('{', start)
    depth = 1
    end = body + 1
    while depth:
        depth += (source[end] == '{') - (source[end] == '}')
        end += 1
    return source[start:end]


compat = (ROOT / 'kunxcore/src/linux_compat.rs').read_text(encoding='utf-8')
pkg = (ROOT / 'kunxcore/src/pkg.rs').as_posix()
sync = (ROOT / 'shared/sync/mod.rs').as_posix()
host = (ROOT / 'sync_tests/src/host.rs').as_posix()
code = r'''
#![allow(dead_code, unused_unsafe)]
use std::sync::atomic::{AtomicU64, Ordering};
static SLEPT: AtomicU64 = AtomicU64::new(0);
struct TrapFrame { x: [u64; 32] }
const EFAULT: i64 = -14;
const EINVAL: i64 = -22;
const EPERM: i64 = -1;
const ENOMEM: i64 = -12;
const PROT_READ: u32 = 1;
const PROT_WRITE: u32 = 2;
const PROT_EXEC: u32 = 4;
const POLLIN: i16 = 1;
const POLLNVAL: i16 = 0x20;
const PPOLL_MAX: usize = 256;
fn user_ok(a: u64, _: usize) -> bool { a > 4096 }
unsafe fn copy_from_user(a: u64, b: &mut [u8]) -> usize {
    std::ptr::copy_nonoverlapping(a as *const u8, b.as_mut_ptr(), b.len()); b.len()
}
unsafe fn copy_to_user(a: u64, b: &[u8]) -> usize {
    std::ptr::copy_nonoverlapping(b.as_ptr(), a as *mut u8, b.len()); b.len()
}
fn uart_rx_pending() -> bool { false }
mod timer { pub fn now_us() -> u64 { 0 } }
mod sched_cfs {
    pub fn sleep_until(v: u64) { super::SLEPT.store(v, super::Ordering::SeqCst); }
}
mod mmu { pub fn uaccess_begin() {} pub fn uaccess_end() {} }
mod process {
    pub fn get_fd(_: i32) -> Option<()> { None }
    pub fn mprotect_page(_: usize, _: u64) -> bool { true }
}
mod vm {
    pub const PAGE_SIZE: usize = 4096;
    pub const AP_USER_RW: u64 = 0x16;
    pub const AP_USER_RO: u64 = 0x12;
    pub fn user_va_range_ok(_: usize, _: usize) -> bool { true }
}
mod fs_simple { pub const T_CHR: u16 = 3; }
mod vfs {
    pub const O_WRONLY: u32 = 1;
    pub const O_CREAT: u32 = 64;
    pub const O_TRUNC: u32 = 512;
    pub fn fstat(_: i32) -> Option<(u16, u64)> { None }
    pub fn open(_: &str, _: u32) -> i32 { -1 }
    pub fn write(_: i32, _: &[u8]) -> i32 { -1 }
    pub fn close(_: i32) -> i32 { 0 }
    pub fn mkdir(_: &str) -> i32 { 0 }
    pub fn unlink(_: &str) -> i32 { 0 }
    pub fn readdir(_: &str, _: usize, _: &mut [u8]) -> i32 { -1 }
}
fn uart_send_str(_: &str) {}
__FUNCTIONS__
#[path = "__PKG__"] mod pkg;
#[path = "__SYNC__"] mod sync;
#[path = "__HOST__"] mod host;
mod actual_flags {
    type MapFlags = u64;
    __FLAGS__
    pub fn changed_text_flags() -> u64 {
        let old_flags = FLAG_USER_CODE;
        let ap = AP_USER_RW;
        __FLAGS_EXPR__
    }
    pub fn has_write_execute(v: u64) -> bool { v & (W | X) == W | X }
}

#[test]
fn mprotect_flag_expression_retains_execute_when_adding_write() {
    assert!(actual_flags::has_write_execute(actual_flags::changed_text_flags()));
}

#[test]
fn pselect_reads_tp_instead_of_timeout_argument() {
    SLEPT.store(0, Ordering::SeqCst);
    let timeout = [0u64, 0];
    let mut f = TrapFrame { x: [0; 32] };
    f.x[14] = timeout.as_ptr() as u64;
    assert_eq!(sys_poll_family(&mut f, 4), 0);
    assert_eq!(SLEPT.load(Ordering::SeqCst), 50_000);
}
#[test]
fn ppoll_zero_fds_skips_requested_sleep() {
    SLEPT.store(0, Ordering::SeqCst);
    let timeout = [1u64, 0];
    let mut f = TrapFrame { x: [0; 32] };
    f.x[12] = timeout.as_ptr() as u64;
    assert_eq!(sys_ppoll(&mut f), 0);
    assert_eq!(SLEPT.load(Ordering::SeqCst), 0);
}
#[test]
fn ppoll_negative_fd_keeps_stale_revents() {
    let mut fd = [-1i32, 0x0001_0001]; // fd=-1, events=1, revents=1
    let timeout = [0u64, 0];
    let mut f = TrapFrame { x: [0; 32] };
    f.x[10] = fd.as_mut_ptr() as u64;
    f.x[11] = 1;
    f.x[12] = timeout.as_ptr() as u64;
    assert_eq!(sys_ppoll(&mut f), 0);
    assert_eq!((fd[1] >> 16) & 0xffff, 1);
}
#[test]
fn clock_nanosleep_dispatch_uses_clock_id_as_pointer() {
    assert!(__CLOCK_ROUTE__);
    let timeout = [0u64, 1_000_000];
    let mut f = TrapFrame { x: [0; 32] };
    f.x[10] = 1; // CLOCK_MONOTONIC
    f.x[12] = timeout.as_ptr() as u64;
    assert_eq!(sys_nanosleep(&mut f), EFAULT);
}
#[test]
fn mprotect_none_succeeds_without_validating_address() {
    let mut f = TrapFrame { x: [0; 32] };
    f.x[10] = 0xffff_ffff_ffff_f000;
    f.x[11] = 4096;
    assert_eq!(sys_mprotect(&mut f), 0);
}
#[test]
fn legacy_package_can_be_modified_and_resigned_without_a_key() {
    let mut b = [0u8; 256];
    let n = pkg::build("audit", b"ELF", "test", &[], &mut b);
    let p = pkg::parse(&b[..n]).unwrap();
    assert!(pkg::verify(&b[..n], &p));
    b[p.elf_off] ^= 1;
    assert!(!pkg::verify(&b[..n], &p));
    let sig = pkg::sign(&b[32..n-32]);
    b[n-32..n].copy_from_slice(&sig);
    assert!(pkg::verify(&b[..n], &p));
}
#[test]
fn hostfs_second_blob_chunk_erases_first_chunk() {
    use sync::fs::Fs;
    let root = std::env::temp_dir().join(format!("kunx-audit-{}", std::process::id()));
    let mut fs = host::HostFs::new(root);
    assert!(fs.write_file_at(b"blob", 0, b"ABC"));
    assert!(fs.write_file_at(b"blob", 3, b"DEF"));
    let mut got = [0; 6];
    assert_eq!(fs.read_file(b"blob", &mut got), 6);
    assert_eq!(&got, b"\0\0\0DEF");
}
#[test]
fn store_update_saves_old_offsets_to_index() {
    use sync::store::{Rec, Store};
    let mut fs = host::MemFs::new();
    let mut buf = [0; 64];
    let mut s = Store::new(&mut buf, b"idx", b"blob");
    let mut r = Rec::new();
    r.key[0] = b'a'; r.key_len = 1;
    assert!(s.upsert(r, b"OLD", true));
    assert!(s.save(&mut fs));
    assert!(s.upsert(r, b"NEW!", true));
    assert!(s.save(&mut fs));
    assert_eq!(s.blob_of(0), Some(&b"NEW!"[..]));
    let mut buf2 = [0; 64];
    let mut reloaded = Store::new(&mut buf2, b"idx", b"blob");
    assert!(reloaded.load(&mut fs));
    assert_ne!(reloaded.blob_of(0), Some(&b"NEW!"[..]));
}
'''
functions = '\n'.join(function(compat, n) for n in ['sys_poll_family', 'sys_ppoll', 'sys_nanosleep', 'sys_mprotect'])
code = code.replace('__FUNCTIONS__', functions).replace('__PKG__', pkg).replace('__SYNC__', sync).replace('__HOST__', host)
code = code.replace('__CLOCK_ROUTE__', str('SYS_CLOCK_NANOSLEEP => sys_nanosleep(f)' in compat).lower())
vm = (ROOT / 'kunxcore/src/vm.rs').read_text(encoding='utf-8')
names = ['R', 'W', 'X', 'U', 'A', 'D', 'AF', 'AP_USER_RW', 'AP_USER_RO', 'FLAG_USER_CODE', 'AP_MASK', 'FLAG_COW']
flags = '\n'.join(re.search(r'^(?:pub )?const ' + n + r':[^\n]+;', vm, re.M).group() for n in names)
process = (ROOT / 'kunxcore/src/process.rs').read_text(encoding='utf-8')
expr = re.search(r'let new_flags = (\(p\.pt\.user_pages\[i\]\.flags[^;]+);', process).group(1)
expr = expr.replace('p.pt.user_pages[i].flags', 'old_flags').replace('vm::', '')
code = code.replace('__FLAGS__', flags).replace('__FLAGS_EXPR__', expr)
source = OUT / 'findings.rs'
source.write_text(code, encoding='utf-8')
exe = OUT / 'findings.exe'
subprocess.run(['rustc', '--edition=2021', '--test', '-C', 'overflow-checks=off', str(source), '-o', str(exe)], cwd=ROOT, check=True)
subprocess.run([str(exe), '--test-threads=1'], cwd=ROOT, check=True)
