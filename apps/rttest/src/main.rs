#![no_std]
#![no_main]
extern crate alloc;
use alloc::{vec, vec::Vec, format};
use core::alloc::Layout;
use kunxrt::{heap, process, syscall::*};
kunxrt::allocator!();
kunxrt::entry!();
kunxrt::panic_handler!("rttest");

fn print(s: &str) { process::write_all(1, s.as_bytes()).unwrap(); }
fn font_batch() {
    let font = kunxfont::Font::from_bytes(include_bytes!("../../../rootfs/fonts/cn.ttf")).unwrap();
    for px in [12., 16., 28.] {
        for ch in "自研运行库内存回收ABC0123".chars() {
            let (m, bitmap) = font.rasterize(ch, px).unwrap();
            assert_eq!(bitmap.len(), m.width * m.height);
        }
    }
}

#[no_mangle]
pub extern "C" fn k_main(argc: usize, argv: *const *const u8) -> usize {
    assert!(argc >= 1 && !argv.is_null());
    assert!(!unsafe { *argv }.is_null());
    print("[runtime-selftest] start\n");
    unsafe {
        assert_eq!(result(syscall0(usize::MAX)), Err(Errno(38)));
        for align in [1, 8, 16, 64, 4096, 65536] {
            let layout = Layout::from_size_align(337, align).unwrap();
            let p = alloc::alloc::alloc_zeroed(layout);
            assert!(!p.is_null());
            assert_eq!(p as usize % align, 0);
            assert!((0..337).all(|i| *p.add(i) == 0));
            kunxrt::memory::memset(p, 0x5a, 337);
            let q = alloc::alloc::realloc(p, layout, 8193);
            assert!(!q.is_null());
            assert_eq!(q as usize % align, 0);
            assert!((0..337).all(|i| *q.add(i) == 0x5a));
            alloc::alloc::dealloc(q, Layout::from_size_align(8193, align).unwrap());
        }
    }
    let mut slots: Vec<Vec<u8>> = (0..97).map(|_| Vec::new()).collect();
    let mut rng = 123u64;
    for step in 0..20000 {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
        let i = (rng >> 32) as usize % slots.len();
        if !slots[i].is_empty() {
            let value = slots[i][0];
            assert!(slots[i].iter().all(|v| *v == value));
            slots[i] = Vec::new();
        } else { slots[i] = vec![step as u8; (rng as usize % 8192)+1]; }
    }
    drop(slots);
    let baseline = heap::stats();
    assert_eq!(baseline.live, 0);
    assert_eq!(baseline.free, baseline.mapped);
    assert_eq!(baseline.largest_free, baseline.free);
    print("[runtime-selftest] allocator churn PASS\n");
    // The child must be able to use the copied allocator after fork.
    let live = vec![0x37u8; 4096];
    let pid = unsafe { process::fork() } as isize;
    assert!(pid >= 0);
    if pid == 0 {
        assert!(live.iter().all(|v| *v == 0x37));
        let child = vec![0x81u8; 16384];
        assert_eq!(child[1234], 0x81);
        drop(child);
        drop(live);
        process::exit(0);
    }
    let mut status = -1i32;
    assert_eq!(unsafe { syscall4(260, pid as usize, &mut status as *mut _ as usize, 0, 0) }, pid as usize);
    assert_eq!(status, 0);
    assert_eq!(live[0], 0x37);
    drop(live);
    print("[runtime-selftest] fork PASS\n");
    font_batch();
    let warm = heap::stats();
    for _ in 0..100 { font_batch(); }
    let after = heap::stats();
    assert_eq!(after.live, 0);
    assert_eq!(after.allocated, 0);
    assert_eq!(after.mapped, warm.mapped);
    assert_eq!(after.largest_free, after.mapped);
    print(&format!("[runtime-selftest] font reuse PASS cycles=100 mapped={} free={}\n", after.mapped, after.free));
    print("[runtime-selftest] PASS\n");
    0
}
