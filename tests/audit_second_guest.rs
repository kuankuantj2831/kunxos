//! Regression checks for the second audit. Isolated image only.
#![no_std]
#![no_main]
use core::arch::{asm,global_asm};
unsafe fn call(n:usize,a:[usize;6])->isize {
    let r:isize;
    asm!("ecall",in("a7") n,inlateout("a0") a[0]=>r,in("a1") a[1],in("a2") a[2],in("a3") a[3],in("a4") a[4],in("a5") a[5],options(nostack)); r
}
fn print(s:&str) { unsafe { call(64,[1,s.as_ptr() as usize,s.len(),0,0,0]); } }
fn check(ok:bool,label:&str) {
    if !ok { print("AUDIT GUEST FAIL: unexpected observation: ");print(label);print("\n");unsafe { call(93,[1,0,0,0,0,0]); } loop {} }
}
const CWD:usize=(-100isize) as usize;
unsafe fn open(path:&[u8],flags:usize)->usize {
    let fd=call(56,[CWD,path.as_ptr() as usize,flags,0,0,0]);check(fd>=0,"open fixture");fd as usize
}
#[no_mangle]
pub extern "C" fn k_main()->usize {
    print("AUDIT2 REGRESSION START\n");
    unsafe {
        let fd=open(b"/tmp/audit2-vectors\0",0x242);
        let a=[b'A';8192]; let b=[b'B';4];
        let iov=[0usize,0,a.as_ptr() as usize,a.len(),b.as_ptr() as usize,b.len()];
        check(call(66,[fd,iov.as_ptr() as usize,3,0,0,0])==8196,"complete writev");
        check(call(62,[fd,4096,0,0,0,0])==4096,"seek writev contents");
        let mut tail=[0u8;4];
        check(call(63,[fd,tail.as_mut_ptr() as usize,4,0,0,0])==4 && tail==[b'A';4],"writev preserves middle bytes");
        call(62,[fd,0,0,0,0,0]);
        let mut first=[0u8;8192];let mut second=[0u8;4];
        let iov=[0usize,0,first.as_mut_ptr() as usize,first.len(),second.as_mut_ptr() as usize,second.len()];
        check(call(65,[fd,iov.as_ptr() as usize,3,0,0,0])==8196,"complete readv");
        check(first==a && second==b,"readv fills first iovec before second");
        call(62,[fd,8192,0,0,0,0]);
        let mut at_eof=[0u8;8];let mut untouched=[7u8;4];
        let iov=[at_eof.as_mut_ptr() as usize,8,untouched.as_mut_ptr() as usize,4];
        check(call(65,[fd,iov.as_ptr() as usize,2,0,0,0])==4 && at_eof[..4]==b && untouched==[7;4],"readv EOF stays in current element");
        let partial=[b"C".as_ptr() as usize,1,1usize,1];
        check(call(66,[fd,partial.as_ptr() as usize,2,0,0,0])==1,"writev fault returns completed prefix");
        print("AUDIT2 vectors PASS\n");

        let duplicate=call(23,[fd,0,0,0,0,0]);check(duplicate>=0,"dup fixture");
        let minfd=call(25,[fd,0,10,0,0,0]);check(minfd>=10,"F_DUPFD lower bound");
        check(call(25,[fd,3,0,0,0,0])==2,"F_GETFL access mode");
        check(call(25,[12345,3,0,0,0,0])==-9,"F_GETFL invalid fd");
        check(call(25,[fd,0,16,0,0,0])==-22,"F_DUPFD invalid minimum");
        check(call(25,[fd,999,0,0,0,0])==-38,"unsupported fcntl");
        check(call(25,[fd,4,0x400,0,0,0])==0,"F_SETFL append");
        check(call(25,[duplicate as usize,3,0,0,0,0])==0x402,"status shared by duplicate");
        call(62,[fd,0,0,0,0,0]);
        check(call(64,[fd,b"Z".as_ptr() as usize,1,0,0,0])==1,"append write");
        check(call(62,[fd,0,2,0,0,0])==8198,"append extends file");
        call(62,[fd,0,0,0,0,0]);let mut byte=[0u8;1];call(63,[fd,byte.as_mut_ptr() as usize,1,0,0,0]);
        check(byte[0]==b'A',"append preserves prefix");
        check(call(25,[fd,4,2,0,0,0])==0 && call(25,[fd,3,0,0,0,0])==2,"F_SETFL clears append without changing access");
        call(57,[duplicate as usize,0,0,0,0,0]);call(57,[minfd as usize,0,0,0,0,0]);
        call(57,[fd,0,0,0,0,0]);
        print("AUDIT2 fcntl PASS\n");

        let wo=open(b"/tmp/audit2-vectors\0",1);
        let mut dest=[0u8;1];let iov=[dest.as_mut_ptr() as usize,1];
        check(call(65,[wo,iov.as_ptr() as usize,1,0,0,0])<0,"readv propagates error");
        call(57,[wo,0,0,0,0,0]);
        let ro=open(b"/tmp/audit2-vectors\0",0);
        check(call(46,[ro,0,0,0,0,0])==-9,"ftruncate readonly rejected");
        check(call(62,[ro,0,2,0,0,0])==8198,"readonly ftruncate preserves contents");
        call(57,[ro,0,0,0,0,0]);
        let rw=open(b"/tmp/audit2-vectors\0",2);
        check(call(46,[rw,0,0,0,0,0])==0 && call(62,[rw,0,2,0,0,0])==0,"writable truncate still works");
        call(57,[rw,0,0,0,0,0]);

        let mut pipes=[0i32;2];check(call(59,[pipes.as_mut_ptr() as usize,0,0,0,0,0])==0,"pipe fixture");
        check(call(64,[pipes[0] as usize,b"Q".as_ptr() as usize,1,0,0,0])==-9,"reject write to read-end");
        check(call(63,[pipes[1] as usize,dest.as_mut_ptr() as usize,1,0,0,0])==-9,"reject read from write-end");
        check(call(64,[pipes[1] as usize,b"Q".as_ptr() as usize,1,0,0,0])==1,"valid pipe write");
        check(call(63,[pipes[0] as usize,dest.as_mut_ptr() as usize,1,0,0,0])==1 && dest[0]==b'Q',"valid pipe read");
        let wrong=[b"Q".as_ptr() as usize,1];
        check(call(66,[pipes[0] as usize,wrong.as_ptr() as usize,1,0,0,0])==-9,"writev pipe endpoint");
        let wrong=[dest.as_mut_ptr() as usize,1];
        check(call(65,[pipes[1] as usize,wrong.as_ptr() as usize,1,0,0,0])==-9,"readv pipe endpoint");
        call(57,[pipes[0] as usize,0,0,0,0,0]);call(57,[pipes[1] as usize,0,0,0,0,0]);
        print("AUDIT2 permissions PASS\n");
    }
    print("AUDIT2 REGRESSION PASS\n");0
}
global_asm!(".section .text._start, \"ax\"",".global _start","_start:","call k_main","li a7,93","ecall","j .");
#[panic_handler]
fn panic(_: &core::panic::PanicInfo)->! { print("AUDIT GUEST FAIL: panic\n");loop {} }
