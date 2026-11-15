//! Run in an isolated QEMU image using tools/test_audit_guest.py.
#![no_std]
#![no_main]
use core::arch::{asm, global_asm};

unsafe fn call(n: usize, args: [usize;6]) -> isize {
    let result: isize;
    asm!("ecall", in("a7") n, inlateout("a0") args[0] => result,
        in("a1") args[1], in("a2") args[2], in("a3") args[3], in("a4") args[4], in("a5") args[5], options(nostack));
    result
}
fn print(s: &str) { unsafe { call(64,[1,s.as_ptr() as usize,s.len(),0,0,0]); } }
fn check(ok: bool, label: &str) {
    if !ok {
        print("AUDIT GUEST FAIL: "); print(label); print("\n");
        unsafe { call(93,[1,0,0,0,0,0]); }
        loop {}
    }
}
fn now() -> u64 {
    let mut t=[0u64;2];
    check(unsafe { call(113,[1,t.as_mut_ptr() as usize,0,0,0,0]) }==0,"clock_gettime");
    t[0]*1_000_000+t[1]/1000
}
unsafe fn map(addr: usize,len:usize,prot:usize,flags:usize)->isize { call(222,[addr,len,prot,flags,usize::MAX,0]) }
unsafe fn protect(addr:usize,prot:usize)->isize { call(226,[addr,4096,prot,0,0,0]) }
unsafe fn child_fault(addr:usize,write:bool,execute:bool) {
    let pid=call(220,[17,0,0,0,0,0]);
    check(pid>=0,"fork fault probe");
    if pid==0 {
        if execute { let f: extern "C" fn() -> usize=core::mem::transmute(addr); let _=f(); }
        else if write { core::ptr::write_volatile(addr as *mut u8,99); }
        else { let _=core::ptr::read_volatile(addr as *const u8); }
        call(93,[0,0,0,0,0,0]); loop {}
    }
    let mut status=0u32;
    check(call(260,[pid as usize,&mut status as *mut _ as usize,0,0,0,0])==pid,"wait fault probe");
    check(status&0x7f!=0,"forbidden access must terminate child");
}
extern "C" { fn audit_exec() -> usize; }

#[no_mangle]
pub extern "C" fn k_main() -> usize {
    print("AUDIT GUEST START\n");
    unsafe {
        let timeout=[0u64,120_000_000];
        let t=now();
        check(call(115,[1,0,timeout.as_ptr() as usize,0,0,0])==0,"clock_nanosleep ABI");
        check(now()-t>=120_000,"clock_nanosleep duration");
        let t=now();
        check(call(73,[0,0,timeout.as_ptr() as usize,0,0,0])==0,"ppoll no descriptors");
        check(now()-t>=120_000,"ppoll must not return at 50ms");
        let zero=[0u64;2]; let mut p=[-1i32,0x00010001];
        check(call(73,[p.as_mut_ptr() as usize,1,zero.as_ptr() as usize,0,0,0])==0 && p[1]>>16==0,"negative pollfd");
        check(call(72,[0,0,0,0,zero.as_ptr() as usize,0])==0,"pselect timeout ABI");
        let invalid=[0u64,1_000_000_000];
        check(call(101,[invalid.as_ptr() as usize,0,0,0,0,0])==-22,"invalid timespec");
        let mut pair=[0i32;2];
        check(call(199,[1,1,0,pair.as_mut_ptr() as usize,0,0])==0,"socketpair");
        check(call(64,[pair[1] as usize,b"hi".as_ptr() as usize,2,0,0,0])==2,"pair write");
        let mut readable=[pair[0],5];
        check(call(73,[readable.as_mut_ptr() as usize,1,zero.as_ptr() as usize,0,0,0])==1 && readable[1]>>16==5,"pair read/write readiness");
        let mut writefds=1u64 << pair[1];
        check(call(72,[pair[1] as usize+1,0,&mut writefds as *mut _ as usize,0,zero.as_ptr() as usize,0])==1,"select writefds");
        call(57,[pair[0] as usize,0,0,0,0,0]); call(57,[pair[1] as usize,0,0,0,0,0]);
        print("AUDIT GUEST timers PASS\n");

        let a=0x2800_0000usize;
        check(map(a+4096,4096,0,0x100022)==(a+4096) as isize,"reserve PROT_NONE");
        check(map(a,8192,3,0x100022)==-17,"partial lazy NOREPLACE collision");
        check(protect(a+4096,3)==0,"restore lazy NONE");
        core::ptr::write_volatile((a+4096) as *mut u8,0x5a);
        check(protect(a+4096,0)==0,"PROT_NONE");
        check(call(113,[1,a+4096,0,0,0,0])==-14,"kernel output to NONE");
        child_fault(a+4096,false,false);
        check(protect(a+4096,3)==0,"restore materialized NONE");
        check(core::ptr::read_volatile((a+4096) as *const u8)==0x5a,"NONE preserved contents");
        check(protect(a+4096,1)==0,"make readonly");
        child_fault(a+4096,true,false);
        check(core::ptr::read_volatile((a+4096) as *const u8)==0x5a,"readonly fork lifetime");
        check(protect(a+4096,3)==0,"readonly to private writable");
        check(protect(a+4096,7)==-1,"reject RWX");
        check(call(215,[a+4096,4096,0,0,0,0])==0,"unmap");
        check(map(a+4096,4096,1,0x100022)==(a+4096) as isize,"unmap frees reservation");
        check(call(113,[1,a+4096,0,0,0,0])==-14,"output to lazy readonly");
        child_fault(a+4096,true,false);
        print("AUDIT GUEST pages PASS\n");

        let text=audit_exec as *const () as usize;
        check(audit_exec()==7,"initial executable leaf");
        check(protect(text,3)==0,"code to writable data");
        child_fault(text,false,true);
        check(call(113,[1,b"READONLY TEXT BUFFER".as_ptr() as usize,0,0,0,0])==-14,"output to ELF rodata");
        let long=[b'x';128];
        check(call(56,[(-100isize) as usize,long.as_ptr() as usize,0,0,0,0])==-36,"unterminated path");
        check(call(281,[3,b"relative\0".as_ptr() as usize,0,0,0,0])==-38,"execveat relative dirfd");
        let path=b"/tmp/audit-exclusive\0";
        let fd=call(56,[(-100isize) as usize,path.as_ptr() as usize,0x41,0,0,0]);
        check(fd>=0,"create exclusive fixture");
        check(call(64,[fd as usize,b"keep".as_ptr() as usize,4,0,0,0])==4,"exclusive fixture write");
        call(57,[fd as usize,0,0,0,0,0]);
        check(call(56,[(-100isize) as usize,path.as_ptr() as usize,0x2c1,0,0,0])==-17,"EXCL precedes TRUNC");
        check(call(56,[3,b"relative\0".as_ptr() as usize,0,0,0,0])==-38,"openat unsupported dirfd");
        let pid=call(220,[17,0,0,0,0,0]);
        check(pid>=0,"fork execveat");
        if pid==0 {
            check(call(49,[b"/bin\0".as_ptr() as usize,0,0,0,0,0])==0,"chdir before execveat");
            let argv=[b"busybox\0".as_ptr() as usize,b"true\0".as_ptr() as usize,0];
            call(281,[(-100isize) as usize,b"busybox\0".as_ptr() as usize,argv.as_ptr() as usize,0,0,0]);
            check(false,"execveat returned after success");
        }
        let mut status=0u32;
        check(call(260,[pid as usize,&mut status as *mut _ as usize,0,0,0,0])==pid && status==0,"execveat relative AT_FDCWD");
        print("AUDIT GUEST permissions PASS\n");
    }
    print("AUDIT GUEST PASS\n");
    0
}

global_asm!(
    ".section .text._start, \"ax\"", ".global _start", "_start:",
    "call k_main", "li a7, 93", "ecall", "j .",
    // A dedicated page lets mprotect remove X without touching the test itself.
    ".section .text.audit, \"ax\"", ".balign 4096", ".global audit_exec",
    "audit_exec:", "li a0,7", "ret", ".space 4096",
);
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { print("AUDIT GUEST FAIL: panic\n"); loop {} }
