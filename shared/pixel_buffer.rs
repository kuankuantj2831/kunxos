//! Demand-paged pixel storage. The virtual capacity stays fixed; the visible
//! slice follows the caller's current layout, so snapshots cannot copy tails.
use core::{cell::UnsafeCell, ops::{Deref, DerefMut}};

pub struct PixelBuffer {
    ptr: UnsafeCell<*mut u32>,
    len: usize,
    capacity: usize,
}

impl PixelBuffer {
    pub const fn new(capacity: usize, len: usize) -> Self {
        Self { ptr: UnsafeCell::new(core::ptr::null_mut()), len, capacity }
    }

    pub fn resize_len(&mut self, len: usize) { assert!(len <= self.capacity); self.len = len; }

    #[inline(always)]
    fn pointer(&self) -> *mut u32 {
        unsafe {
            if (*self.ptr.get()).is_null() {
                let address: usize;
                core::arch::asm!("ecall", in("a7") 222usize,
                    inlateout("a0") 0usize => address, in("a1") self.capacity * 4,
                    in("a2") 3usize, in("a3") 0x22usize,
                    in("a4") usize::MAX, in("a5") 0usize, options(nostack));
                assert!((address as isize) > 0, "pixel buffer mmap failed");
                *self.ptr.get() = address as *mut u32;
            }
            *self.ptr.get()
        }
    }
}

impl Deref for PixelBuffer {
    type Target = [u32];
    #[inline(always)]
    fn deref(&self) -> &[u32] {
        let len = self.len;
        assert!(len <= self.capacity);
        unsafe { core::slice::from_raw_parts(self.pointer(), len) }
    }
}

impl DerefMut for PixelBuffer {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut [u32] {
        let len = self.len;
        assert!(len <= self.capacity);
        unsafe { core::slice::from_raw_parts_mut(self.pointer(), len) }
    }
}
