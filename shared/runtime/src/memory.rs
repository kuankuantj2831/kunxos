//! C ABI memory primitives. Volatile operations prevent optimizer-generated
//! memcpy/memset calls from recursively reentering these implementations.

#[cfg_attr(all(feature = "memory-symbols", target_os = "none"), no_mangle)]
pub unsafe extern "C" fn memcpy(dest: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    let (mut d, mut s, mut left) = (dest, src, n);
    if (d as usize & 7) == (s as usize & 7) {
        while left != 0 && d as usize & 7 != 0 {
            d.write_volatile(s.read_volatile()); d = d.add(1); s = s.add(1); left -= 1;
        }
        while left >= 16 {
            let a = (s as *const u64).read_volatile();
            let b = (s.add(8) as *const u64).read_volatile();
            (d as *mut u64).write_volatile(a); (d.add(8) as *mut u64).write_volatile(b);
            d = d.add(16); s = s.add(16); left -= 16;
        }
        while left >= 8 {
            (d as *mut u64).write_volatile((s as *const u64).read_volatile());
            d = d.add(8); s = s.add(8); left -= 8;
        }
    } else if (d as usize & 3) == 0 && (s as usize & 3) == 0 {
        while left >= 4 {
            (d as *mut u32).write_volatile((s as *const u32).read_volatile());
            d = d.add(4); s = s.add(4); left -= 4;
        }
    }
    while left != 0 {
        d.write_volatile(s.read_volatile()); d = d.add(1); s = s.add(1); left -= 1;
    }
    dest
}

#[cfg_attr(all(feature = "memory-symbols", target_os = "none"), no_mangle)]
pub unsafe extern "C" fn memmove(dest: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    if dest as usize <= src as usize || (dest as usize).wrapping_sub(src as usize) >= n {
        return memcpy(dest, src, n);
    }
    let mut left = n;
    if (dest as usize & 7) == (src as usize & 7) {
        while left != 0 && (dest as usize + left) & 7 != 0 {
            left -= 1; dest.add(left).write_volatile(src.add(left).read_volatile());
        }
        while left >= 8 {
            left -= 8;
            (dest.add(left) as *mut u64).write_volatile((src.add(left) as *const u64).read_volatile());
        }
    }
    while left != 0 { left -= 1; dest.add(left).write_volatile(src.add(left).read_volatile()); }
    dest
}

#[cfg_attr(all(feature = "memory-symbols", target_os = "none"), no_mangle)]
pub unsafe extern "C" fn memset(dest: *mut u8, value: i32, n: usize) -> *mut u8 {
    let byte = value as u8;
    let word = u64::from_ne_bytes([byte; 8]);
    let mut d = dest; let mut left = n;
    while left != 0 && d as usize & 7 != 0 { d.write_volatile(byte); d = d.add(1); left -= 1; }
    while left >= 16 {
        (d as *mut u64).write_volatile(word); (d.add(8) as *mut u64).write_volatile(word);
        d = d.add(16); left -= 16;
    }
    while left >= 8 { (d as *mut u64).write_volatile(word); d = d.add(8); left -= 8; }
    while left != 0 { d.write_volatile(byte); d = d.add(1); left -= 1; }
    dest
}

#[cfg_attr(all(feature = "memory-symbols", target_os = "none"), no_mangle)]
pub unsafe extern "C" fn memcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    for i in 0..n {
        let (a, b) = (a.add(i).read_volatile(), b.add(i).read_volatile());
        if a != b { return a as i32 - b as i32; }
    }
    0
}

#[cfg_attr(all(feature = "memory-symbols", target_os = "none"), no_mangle)]
pub unsafe extern "C" fn bcmp(a: *const u8, b: *const u8, n: usize) -> i32 { memcmp(a, b, n) }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn copy_fill_and_compare_all_alignments() {
        let src: std::vec::Vec<u8> = (0..400).map(|i| (i % 251) as u8).collect();
        for a in 0..16 { for b in 0..16 { for n in [0, 1, 7, 8, 15, 16, 31, 64, 257] {
            let mut dst = [0xa5; 400];
            unsafe { memcpy(dst.as_mut_ptr().add(a), src.as_ptr().add(b), n); }
            assert_eq!(&dst[a..a+n], &src[b..b+n]);
            assert!(dst[..a].iter().chain(&dst[a+n..]).all(|x| *x == 0xa5));
            assert_eq!(unsafe { memcmp(dst.as_ptr().add(a), src.as_ptr().add(b), n) }, 0);
            unsafe { memset(dst.as_mut_ptr().add(a), 0x180, n); }
            assert!(dst[a..a+n].iter().all(|x| *x == 0x80));
        } } }
        assert!(unsafe { memcmp(b"a".as_ptr(), b"b".as_ptr(), 1) } < 0);
    }
    #[test]
    fn overlap_matches_copy_within() {
        for a in 0..40 { for b in 0..40 { for n in [0, 1, 8, 31, 128, 257] {
            let mut actual: std::vec::Vec<u8> = (0..400).map(|i| i as u8).collect();
            let mut expected = actual.clone(); expected.copy_within(b..b+n, a);
            unsafe { memmove(actual.as_mut_ptr().add(a), actual.as_ptr().add(b), n); }
            assert_eq!(actual, expected);
        } } }
    }
}
