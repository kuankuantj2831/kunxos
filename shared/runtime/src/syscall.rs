//! Raw asm-generic syscall ABI. Errors remain their original negative values.

#[inline(always)]
pub unsafe fn syscall6(n: usize, a0: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> usize {
    #[cfg(target_arch = "riscv64")]
    {
        let result: usize;
        // No nomem/readonly: a syscall may read or change pointed-to memory.
        core::arch::asm!("ecall", in("a7") n, inlateout("a0") a0 => result,
            in("a1") a1, in("a2") a2, in("a3") a3, in("a4") a4, in("a5") a5,
            options(nostack));
        result
    }
    #[cfg(not(target_arch = "riscv64"))]
    { let _ = (n, a0, a1, a2, a3, a4, a5); (-38isize) as usize }
}

#[inline(always)]
pub unsafe fn syscall0(n: usize) -> usize { syscall6(n, 0, 0, 0, 0, 0, 0) }
#[inline(always)]
pub unsafe fn syscall1(n: usize, a: usize) -> usize { syscall6(n, a, 0, 0, 0, 0, 0) }
#[inline(always)]
pub unsafe fn syscall2(n: usize, a: usize, b: usize) -> usize { syscall6(n, a, b, 0, 0, 0, 0) }
#[inline(always)]
pub unsafe fn syscall3(n: usize, a: usize, b: usize, c: usize) -> usize { syscall6(n, a, b, c, 0, 0, 0) }
#[inline(always)]
pub unsafe fn syscall4(n: usize, a: usize, b: usize, c: usize, d: usize) -> usize { syscall6(n, a, b, c, d, 0, 0) }
#[inline(always)]
pub unsafe fn syscall5(n: usize, a: usize, b: usize, c: usize, d: usize, e: usize) -> usize { syscall6(n, a, b, c, d, e, 0) }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Errno(pub u16);

/// Decode only the Linux/asm-generic errno range, preserving high valid values.
pub fn result(raw: usize) -> Result<usize, Errno> {
    if raw >= (-4095isize) as usize { Err(Errno(raw.wrapping_neg() as u16)) }
    else { Ok(raw) }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn errno_boundaries() {
        assert_eq!(result(0), Ok(0));
        assert_eq!(result(usize::MAX - 4095), Ok(usize::MAX - 4095));
        assert_eq!(result((-4095isize) as usize), Err(Errno(4095)));
        assert_eq!(result((-2isize) as usize), Err(Errno(2)));
        assert_eq!(result(usize::MAX), Err(Errno(1)));
    }
}
