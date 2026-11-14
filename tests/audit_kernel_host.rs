// Source-extraction host harness used by docs/audit-2026-11-14-repro.py.
// Memory, scheduler and device services below are stubs; syscall/VM functions
// marked with placeholders are loaded from the current implementation.
#![allow(dead_code, unused_unsafe, unused_variables)]
use std::sync::atomic::{AtomicU64, Ordering};
static NOW: AtomicU64 = AtomicU64::new(0);
static RESERVED: AtomicU64 = AtomicU64::new(0);
static REC_FLAGS: AtomicU64 = AtomicU64::new(0);
static PROTECT_CALLS: AtomicU64 = AtomicU64::new(0);
#[derive(Clone)]
struct TrapFrame { x: [u64; 32] }
const EFAULT: i64 = -14;
const EINVAL: i64 = -22;
const EPERM: i64 = -1;
const EBADF: i64 = -9;
const ENOMEM: i64 = -12;
const ENOSYS: i64 = -38;
const ENAMETOOLONG: i64 = -36;
const PROT_READ: u32 = 1;
const PROT_WRITE: u32 = 2;
const PROT_EXEC: u32 = 4;
const POLLIN: i16 = 1;
const POLLOUT: i16 = 4;
const POLLERR: i16 = 8;
const POLLHUP: i16 = 16;
const POLLNVAL: i16 = 32;
const PPOLL_MAX: usize = 256;
const MAX_EXEC_ARGS: usize = 8;
const EXEC_STR_LEN: usize = 64;
use process::FdKind;
fn user_ok(a: u64, _: usize) -> bool { a > 4096 }
fn user_ok_writable(a: u64, n: usize) -> bool { user_ok(a,n) }
unsafe fn copy_from_user(a: u64, b: &mut [u8]) -> usize {
    if !user_ok(a,b.len()) { return 0; }
    std::ptr::copy_nonoverlapping(a as *const u8, b.as_mut_ptr(), b.len()); b.len()
}
unsafe fn copy_to_user(a: u64, b: &[u8]) -> usize {
    if !user_ok_writable(a,b.len()) { return 0; }
    std::ptr::copy_nonoverlapping(b.as_ptr(), a as *mut u8, b.len()); b.len()
}
fn uart_rx_pending() -> bool { false }
fn sys_execve(f: &mut TrapFrame) -> i64 {
    assert_eq!(f.x[11], 42);
    assert_eq!(f.x[12], 43);
    -2 // failed exec must restore the original arguments
}
mod timer { pub fn now_us() -> u64 { super::NOW.load(super::Ordering::SeqCst) } }
mod rtc { pub fn wall_time_us() -> u64 { 1_000_000_000 + super::timer::now_us() } }
mod sched_cfs { pub fn sleep_until(v: u64) { super::NOW.store(v, super::Ordering::SeqCst); } }
mod net_stack { pub fn poll() {} }
mod fs_simple { pub const T_CHR: u16 = 3; }
mod socket { pub fn poll_events(_: i32) -> Option<i16> { None } }
mod ipc {
    pub fn poll_events(_: super::process::FdEntry) -> i16 {
        if super::timer::now_us() >= 100_000 { 1 | 4 } else { 0 }
    }
}
mod process {
    #[derive(Clone,Copy)]
    pub enum FdKind { File, Pipe, Pair, MsgQueue, None }
    pub struct FdEntry { pub kind: FdKind }
    pub fn get_fd(fd: i32) -> Option<FdEntry> { if fd == 3 { Some(FdEntry { kind: FdKind::Pipe }) } else { None } }
    pub fn root_page_reserved(_: usize, page: usize) -> bool { super::RESERVED.load(super::Ordering::SeqCst) == page as u64 }
    pub fn mprotect_page(_: usize, _: u64) -> bool { super::PROTECT_CALLS.fetch_add(1,super::Ordering::SeqCst); true }
}
mod vfs {
    pub const O_WRONLY: u32 = 1;
    pub const O_CREAT: u32 = 64;
    pub const O_TRUNC: u32 = 512;
    pub fn fstat(_: i32) -> Option<(u16, u64)> { Some((1,0)) }
    pub fn open(_: &str, _: u32) -> i32 { panic!("legacy installation must not write") }
    pub fn write(_: i32, _: &[u8]) -> i32 { -1 }
    pub fn close(_: i32) -> i32 { 0 }
    pub fn mkdir(_: &str) -> i32 { 0 }
    pub fn unlink(_: &str) -> i32 { 0 }
    pub fn readdir(_: &str, _: usize, _: &mut [u8]) -> i32 { -1 }
}
fn uart_send_str(_: &str) {}
#[path = "__PKG__"] mod pkg;
mod vm {
    type MapFlags = u64;
    type PhysAddr = usize;
    #[repr(C, align(4096))]
    pub struct PageTable { pub entries: [u64; 512] }
    pub fn user_va_range_ok(va: usize, len: usize) -> bool { va > 0 && va.checked_add(len).map_or(false, |end| end <= 0x40000000) }
    __VM__
}
mod output_buffer {
    use super::vm;
    fn current_root() -> usize { 0 }
    struct Rec { flags: u64, paddr: usize }
    fn lookup_page_idx(_: usize) -> Option<(usize,Rec)> { Some((0,Rec { flags: super::REC_FLAGS.load(super::Ordering::SeqCst), paddr: 0x1000 })) }
    fn page_fault_resolve_at(_: usize,_:usize) -> bool { true }
    fn mprotect_page(va:usize,flags:u64) -> bool { super::process::mprotect_page(va,flags) }
    __WRITABLE__
    pub fn prepare(flags: u64) -> bool {
        super::REC_FLAGS.store(flags,super::Ordering::SeqCst);
        ensure_user_range_writable(0x1000,1)
    }
}
__COMPAT__
fn frame() -> TrapFrame { NOW.store(0,Ordering::SeqCst); TrapFrame { x:[0;32] } }

#[test]
fn clock_nanosleep_uses_req_a2_and_absolute_clock() {
    let t = [0i64, 90_000_000];
    let mut f = frame(); f.x[10]=1; f.x[12]=t.as_ptr() as u64;
    assert_eq!(sys_clock_nanosleep(&mut f),0); assert_eq!(timer::now_us(),90_000);
    let absolute = [1000i64, 120_000_000];
    f.x[10]=0; f.x[11]=1; f.x[12]=absolute.as_ptr() as u64;
    assert_eq!(sys_clock_nanosleep(&mut f),0); assert_eq!(timer::now_us(),120_000);
}
#[test]
fn invalid_timespec_fails_instead_of_sleeping() {
    let mut f=frame();
    for t in [[-1i64,0],[0,-1],[0,1_000_000_000]] {
        f.x[10]=t.as_ptr() as u64; assert_eq!(sys_nanosleep(&mut f),EINVAL);
    }
    assert_eq!(timer::now_us(),0);
}
#[test]
fn ppoll_zero_fds_waits_full_timeout() {
    let mut f=frame(); let t=[0i64,120_000_000]; f.x[12]=t.as_ptr() as u64;
    assert_eq!(sys_ppoll(&mut f),0); assert_eq!(timer::now_us(),120_000);
}
#[test]
fn ppoll_negative_fd_clears_revents() {
    let mut f=frame(); let t=[0i64,0]; let mut p=[-1i32,0x00010001];
    f.x[10]=p.as_mut_ptr() as u64; f.x[11]=1; f.x[12]=t.as_ptr() as u64;
    assert_eq!(sys_ppoll(&mut f),0); assert_eq!(p[1]>>16,0);
}
#[test]
fn ppoll_waits_beyond_old_50ms_limit_for_pipe() {
    let mut f=frame(); let t=[0i64,200_000_000]; let mut p=[3i32,1];
    f.x[10]=p.as_mut_ptr() as u64; f.x[11]=1; f.x[12]=t.as_ptr() as u64;
    assert_eq!(sys_ppoll(&mut f),1); assert_eq!(timer::now_us(),100_000); assert_eq!(p[1]>>16,1);
}
#[test]
fn pselect_handles_writefds_and_original_requests() {
    let mut f=frame(); let t=[0i64,200_000_000]; let mut r=8u64; let mut w=8u64;
    f.x[10]=4; f.x[11]=(&mut r) as *mut _ as u64; f.x[12]=(&mut w) as *mut _ as u64; f.x[14]=t.as_ptr() as u64;
    assert_eq!(sys_poll_family(&mut f,14),2); assert_eq!(r,8); assert_eq!(w,8); assert_eq!(timer::now_us(),100_000);
}
#[test]
fn pselect_zero_timeout_reads_a4() {
    let mut f=frame(); let t=[0i64,0]; f.x[14]=t.as_ptr() as u64;
    assert_eq!(sys_poll_family(&mut f,14),0); assert_eq!(timer::now_us(),0);
}
#[test]
fn mprotect_none_validates_range() {
    let mut f=frame(); f.x[10]=u64::MAX-4095; f.x[11]=4096;
    assert_eq!(sys_mprotect(&mut f),EINVAL);
}
#[test]
fn data_permissions_clear_execute_and_none() {
    let flags=vm::data_permissions(vm::FLAG_USER_CODE | vm::FLAG_NO_ACCESS,vm::AP_USER_RW);
    assert_eq!(flags & (1<<3),0); assert!(vm::user_writable(flags));
    assert_eq!(vm::page_descriptor(0x1000,flags|vm::FLAG_NO_ACCESS),0);
}
#[test]
fn syscall_output_denies_ro_and_none_but_breaks_cow() {
    PROTECT_CALLS.store(0,Ordering::SeqCst);
    assert!(!output_buffer::prepare(vm::FLAG_USER_CODE));
    assert!(!output_buffer::prepare(vm::FLAG_USER_DATA | vm::FLAG_NO_ACCESS));
    assert_eq!(PROTECT_CALLS.load(Ordering::SeqCst),0);
    assert!(output_buffer::prepare(vm::FLAG_USER_DATA | vm::FLAG_COW));
    assert_eq!(PROTECT_CALLS.load(Ordering::SeqCst),1);
}
#[test]
fn sv39_walk_and_unmap_use_tables_not_leaves() {
    let mut root=Box::new(vm::PageTable { entries:[0;512] });
    let mut l2=Box::new(vm::PageTable { entries:[0;512] });
    let mut l3=Box::new(vm::PageTable { entries:[0;512] });
    root.entries[0]=vm::addr_to_ppn((&mut *l2) as *mut _ as usize)|1;
    l2.entries[0]=vm::addr_to_ppn((&mut *l3) as *mut _ as usize)|1;
    l3.entries[1]=vm::page_descriptor(0x2000,vm::FLAG_USER_DATA);
    let addr=(&mut *root) as *mut _ as usize;
    assert!(vm::user_page_pte_u(addr,4096)); assert!(vm::user_page_pte_writable(addr,4096));
    vm::unmap_page_in(addr,4096); assert_eq!(l3.entries[1],0); assert!(!vm::user_page_pte_u(addr,4096));
    root.entries[0]=vm::page_descriptor(0x40000000,vm::FLAG_USER_CODE);
    assert!(vm::user_page_pte_u(addr,4096)); assert!(!vm::user_page_pte_writable(addr,4096));
}
#[test]
fn noreplace_detects_partial_and_inaccessible_reservations() {
    RESERVED.store(8192,Ordering::SeqCst);
    assert!(vm::user_range_occupied(0,4096,8192));
    assert!(!vm::user_range_occupied(0,4096,4096));
    RESERVED.store(0,Ordering::SeqCst);
}
#[test]
fn strings_require_nul_and_complete_user_access() {
    let mut buf=[0u8;4];
    assert_eq!(unsafe { read_cstr(b"abcd".as_ptr() as u64,&mut buf) },Err(ENAMETOOLONG));
    assert_eq!(unsafe { read_cstr(b"abc\0".as_ptr() as u64,&mut buf) },Ok(3));
    assert_eq!(unsafe { read_cstr(1,&mut buf) },Err(EFAULT));
}
#[test]
fn execveat_translates_arguments_and_preserves_error_frame() {
    let mut f=frame(); f.x[10]=(-100i64) as u64; f.x[11]=b"/bin/test\0".as_ptr() as u64; f.x[12]=42; f.x[13]=43;
    let original=f.clone(); assert_eq!(sys_execveat(&mut f),-2); assert_eq!(f.x,original.x);
    f.x[11]=b"relative\0".as_ptr() as u64; f.x[10]=3; assert_eq!(sys_execveat(&mut f),ENOSYS);
}
#[test]
fn forged_legacy_package_is_never_installed() {
    let mut b=[0u8;256]; let n=pkg::build("audit",b"ELF","test",&[],&mut b);
    let p=pkg::parse(&b[..n]).unwrap(); b[p.elf_off]^=1;
    let checksum=pkg::sign(&b[32..n-32]); b[n-32..n].copy_from_slice(&checksum);
    assert!(pkg::verify(&b[..n],&p)); assert_eq!(pkg::install(&p,&b[..n]),-95);
}
