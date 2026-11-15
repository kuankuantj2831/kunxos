//! Address-sorted free list. Freed blocks coalesce and remain mapped for reuse.
use core::{alloc::{GlobalAlloc, Layout}, cell::UnsafeCell, ptr, sync::atomic::{AtomicBool, Ordering}};

const ALIGN: usize = 16;
const HEADER: usize = core::mem::size_of::<Header>();
#[repr(C)]
struct Node { size: usize, next: *mut Node }
#[repr(C)]
struct Header { base: usize, span: usize }

#[derive(Clone, Copy, Debug, Default)]
pub struct HeapStats {
    pub mapped: usize,
    pub allocated: usize,
    pub live: usize,
    pub free: usize,
    pub largest_free: usize,
}

pub struct Heap { head: *mut Node, mapped: usize, allocated: usize, live: usize }
unsafe impl Send for Heap {}
fn round(n: usize, align: usize) -> Option<usize> {
    n.checked_add(align - 1).map(|v| v & !(align - 1))
}
impl Heap {
    pub const fn new() -> Self {
        Self { head: ptr::null_mut(), mapped: 0, allocated: 0, live: 0 }
    }
    /// Add exclusive writable storage which must outlive this heap and all its allocations.
    /// Regions must not overlap previous regions or outstanding allocations.
    pub unsafe fn add_region(&mut self, base: *mut u8, size: usize) {
        let Some(start) = round(base as usize, ALIGN) else { return };
        let Some(end) = (base as usize).checked_add(size) else { return };
        let end = end & !(ALIGN - 1);
        if end <= start || end - start < ALIGN { return; }
        self.mapped += end - start;
        self.insert(start, end - start);
    }
    unsafe fn insert(&mut self, base: usize, size: usize) {
        let mut link = &mut self.head as *mut *mut Node;
        let mut prev: *mut Node = ptr::null_mut();
        while !(*link).is_null() && (*link as usize) < base {
            prev = *link;
            link = &mut (**link).next;
        }
        let node = base as *mut Node;
        node.write(Node { size, next: *link });
        *link = node;
        let next = (*node).next;
        if !next.is_null() && base + (*node).size == next as usize {
            (*node).size += (*next).size;
            (*node).next = (*next).next;
        }
        if !prev.is_null() && prev as usize + (*prev).size == base {
            (*prev).size += (*node).size;
            (*prev).next = (*node).next;
        }
    }
    pub unsafe fn alloc(&mut self, layout: Layout) -> *mut u8 {
        let mut link = &mut self.head as *mut *mut Node;
        while !(*link).is_null() {
            let node = *link;
            let base = node as usize;
            let Some(payload) = base.checked_add(HEADER).and_then(|v| round(v, layout.align().max(ALIGN))) else { return ptr::null_mut() };
            let Some(end) = payload.checked_add(layout.size().max(1)).and_then(|v| round(v, ALIGN)) else { return ptr::null_mut() };
            let span = end - base;
            if span <= (*node).size {
                let total = (*node).size;
                let next = (*node).next;
                let used = if total - span >= ALIGN {
                    let tail = end as *mut Node;
                    tail.write(Node { size: total - span, next });
                    *link = tail;
                    span
                } else { *link = next; total };
                ((payload - HEADER) as *mut Header).write(Header { base, span: used });
                self.allocated += used;
                self.live += 1;
                return payload as *mut u8;
            }
            link = &mut (*node).next;
        }
        ptr::null_mut()
    }
    /// ptr must be a live allocation returned by this heap.
    pub unsafe fn dealloc(&mut self, ptr: *mut u8) {
        let header = ((ptr as usize - HEADER) as *const Header).read();
        self.allocated -= header.span;
        self.live -= 1;
        self.insert(header.base, header.span);
    }
    pub fn stats(&self) -> HeapStats {
        let mut stats = HeapStats { mapped: self.mapped, allocated: self.allocated, live: self.live, ..HeapStats::default() };
        let mut node = self.head;
        unsafe { while !node.is_null() {
            stats.free += (*node).size;
            stats.largest_free = stats.largest_free.max((*node).size);
            node = (*node).next;
        } }
        stats
    }
}

struct State { heap: Heap, end: usize }
struct Shared { locked: AtomicBool, state: UnsafeCell<State> }
unsafe impl Sync for Shared {}
static SHARED: Shared = Shared { locked: AtomicBool::new(false), state: UnsafeCell::new(State { heap: Heap::new(), end: 0 }) };
struct Guard;
impl Drop for Guard { fn drop(&mut self) { SHARED.locked.store(false, Ordering::Release); } }
fn lock() -> Guard {
    while SHARED.locked.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        core::hint::spin_loop();
    }
    Guard
}
pub struct ProcessAllocator;
impl ProcessAllocator { pub const fn new() -> Self { Self } }
pub fn stats() -> HeapStats {
    let _guard = lock();
    unsafe { (*SHARED.state.get()).heap.stats() }
}
unsafe impl GlobalAlloc for ProcessAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _guard = lock();
        let state = &mut *SHARED.state.get();
        let found = state.heap.alloc(layout);
        if !found.is_null() { return found; }
        let Some(need) = layout.size().max(1).checked_add(layout.align().max(ALIGN)).and_then(|v| v.checked_add(HEADER)).and_then(|v| round(v.max(256 * 1024), 4096)) else { return ptr::null_mut() };
        if state.end == 0 {
            let current = crate::syscall::syscall1(214, 0);
            if current == 0 || crate::syscall::result(current).is_err() { return ptr::null_mut(); }
            state.end = current;
        }
        let Some(want) = state.end.checked_add(need).and_then(|v| round(v, 4096)) else { return ptr::null_mut() };
        let got = crate::syscall::syscall1(214, want);
        if got < want || crate::syscall::result(got).is_err() { return ptr::null_mut(); }
        state.heap.add_region(state.end as *mut u8, got - state.end);
        state.end = got;
        state.heap.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, _: Layout) {
        let _guard = lock();
        (*SHARED.state.get()).heap.dealloc(ptr);
    }
}
pub(crate) unsafe fn fork_with_heap_lock() -> usize {
    let _guard = lock();
    crate::syscall::syscall5(220, 17, 0, 0, 0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;
    #[test]
    fn exhaustion_preserves_live_data_and_regions_rejoin() {
        let mut arena = [0u128; 1024];
        let base = arena.as_mut_ptr().cast::<u8>();
        let mut heap = Heap::new();
        unsafe {
            // Insert adjacent regions in reverse order, including unaligned edges.
            heap.add_region(base.add(8192), 8192);
            heap.add_region(base.add(1), 8191);
            assert_eq!(heap.stats().largest_free, 16368);
            let layout = Layout::from_size_align(10000, 64).unwrap();
            let p = heap.alloc(layout);
            assert!(!p.is_null());
            ptr::write_bytes(p, 0x5a, 10000);
            assert!(heap.alloc(layout).is_null());
            assert!((0..10000).all(|i| *p.add(i) == 0x5a));
            heap.dealloc(p);
            let q = heap.alloc(layout);
            assert_eq!(q, p);
            heap.dealloc(q);
            assert_eq!(heap.stats().largest_free, 16368);
        }
        // The rounded-away prefix is not used as a free-list node.
        assert_eq!(arena[0], 0);
    }
    #[test]
    fn churn_alignment_contents_and_coalescing() {
        let mut arena = vec![0u128; 65536];
        let mut heap = Heap::new();
        unsafe { heap.add_region(arena.as_mut_ptr().cast(), arena.len() * 16); }
        let initial = heap.stats().free;
        let mut slots: std::vec::Vec<Option<(*mut u8, usize, u8)>> = vec![None; 127];
        let mut rng = 173u64;
        for _ in 0..30000 {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            let i = (rng >> 32) as usize % slots.len();
            unsafe {
                if let Some((p, size, value)) = slots[i].take() {
                    assert!((0..size).all(|j| *p.add(j) == value));
                    heap.dealloc(p);
                } else {
                    let size = (rng as usize % 2000) + 1;
                    let alignment = 1 << ((rng >> 24) % 13);
                    let p = heap.alloc(Layout::from_size_align(size, alignment).unwrap());
                    assert!(!p.is_null());
                    assert_eq!(p as usize % alignment, 0);
                    let value = rng as u8;
                    ptr::write_bytes(p, value, size);
                    slots[i] = Some((p, size, value));
                }
            }
        }
        for (p, _, _) in slots.into_iter().flatten() { unsafe { heap.dealloc(p); } }
        let stats = heap.stats();
        assert_eq!(stats.live, 0);
        assert_eq!(stats.allocated, 0);
        assert_eq!(stats.free, initial);
        assert_eq!(stats.largest_free, initial);
        unsafe { assert!(heap.alloc(Layout::from_size_align(initial, 16).unwrap()).is_null()); }
        assert!(round(usize::MAX, 16).is_none());
    }
}
