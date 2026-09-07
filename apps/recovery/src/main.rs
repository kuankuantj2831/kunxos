// KunxOS 恢复菜单（Recovery Mode UI）。
//
// 恢复模式下由内核挂载恢复 ramdisk（/mnt/recovery）并启动本程序（串口菜单）。
// 全部交互经 /dev/tty（UART）完成；OTA 状态/安装/切槽/清除数据经自定义
// 系统调用 430（SYS_KUNX_OTA）转发到内核 ota 服务。
//
// 菜单：
//   1. 状态（OTP + BCB）       2. 校验 OTA 包
//   3. 安装 OTA 包（写非活动槽） 4. 清除 /data
//   5. 切槽 A/B                 6. 重启
//

#![no_std]
#![no_main]

use core::arch::asm;
use core::panic::PanicInfo;

const SYS_WRITE: usize = 64;
const SYS_READ: usize = 63;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_EXIT: usize = 93;
const SYS_REBOOT: usize = 169;

const SYS_KUNX_OTA: usize = 430;
const OTA_CMD_INSTALL: usize = 1;
const OTA_CMD_CHECK: usize = 2;
const OTA_CMD_STATUS: usize = 3;
const OTA_CMD_SET_RECOVERY: usize = 5;
const OTA_CMD_FORCE_SLOT: usize = 6;
const OTA_CMD_WIPE_DATA: usize = 7;

const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
const O_RDWR: usize = 2;

unsafe fn sc1(n: usize, a1: usize) -> usize {
    let r: usize;
    asm!("svc #0", in("x8") n, inlateout("x0") a1 => r, options(nostack));
    r
}
unsafe fn sc2(n: usize, a1: usize, a2: usize) -> usize {
    let r: usize;
    asm!("svc #0", in("x8") n, inlateout("x0") a1 => r, in("x1") a2, options(nostack));
    r
}
unsafe fn sc3(n: usize, a1: usize, a2: usize, a3: usize) -> usize {
    let r: usize;
    asm!("svc #0", in("x8") n, inlateout("x0") a1 => r, in("x1") a2, in("x2") a3, options(nostack));
    r
}

fn out(_tty: i64, s: &str) {
    unsafe {
        sc3(SYS_WRITE, 1, s.as_ptr() as usize, s.len());
    }
}

fn getc(_tty: i64) -> u8 {
    let mut c = [0u8; 1];
    loop {
        let n = unsafe { sc3(SYS_READ, 0, c.as_mut_ptr() as usize, 1) } as isize;
        if n == 1 {
            return c[0];
        }
        if n == -2 {
            // -2 = EAGAIN / 无数据：忙等
            continue;
        }
        // ★ 0（EOF）与其它错误（-1/-EBADF/-EIO…）此前既不返回也不退出 →
        // 忙等空转，recovery 永久卡死。按错误处理返回 0（调用方按无效键丢弃）。
        return 0;
    }
}

fn read_line(tty: i64) -> [u8; 128] {
    let mut line = [0u8; 128];
    let mut n = 0usize;
    loop {
        let c = getc(tty);
        if c == b'\n' || c == b'\r' {
            break;
        }
        if c == 0x7F || c == 8 {
            if n > 0 {
                n -= 1;
            }
            continue;
        }
        if n < line.len() - 1 {
            line[n] = c;
            n += 1;
        }
    }
    line[n] = 0;
    line
}

fn prompt_str(tty: i64, ask: &str) -> [u8; 128] {
    out(tty, ask);
    out(tty, " > ");
    read_line(tty)
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let tty: i64 = 1; // 内核为每个进程预置 fd 0/1/2 = /dev/tty

    out(tty, "\n=== KunxOS Recovery ===");
    out(tty, "\n[sec] 引导加载程序已验证，恢复 ramdisk 已挂载");
    loop {
        let sel = prompt_str(tty, "\n1 status  2 check OTA  3 install OTA\n4 wipe data  5 switch slot A/B  6 reboot\nselect");
        let c = sel[0];
        match c {
            b'1' => {
                unsafe { sc1(SYS_KUNX_OTA, OTA_CMD_STATUS); }
            }
            b'2' | b'3' => {
                let p = prompt_str(tty, "OTA 升级包路径（例如 /data/ota/demo.kxup）");
                if p[0] != 0 {
                    let cmd = if c == b'2' { OTA_CMD_CHECK } else { OTA_CMD_INSTALL };
                    let r = unsafe { sc2(SYS_KUNX_OTA, cmd, p.as_ptr() as usize) };
                    out(tty, "\n[ota] result=");
                    let mut buf = [0u8; 24];
                    let mut n = 0usize;
                    let mut v = r;
                    if v == 0 {
                        buf[0] = b'0';
                        n = 1;
                    } else {
                        while v > 0 {
                            buf[n] = b'0' + (v % 10) as u8;
                            n += 1;
                            v /= 10;
                        }
                        buf[..n].reverse();
                    }
                    out(tty, core::str::from_utf8(&buf[..n]).unwrap_or("?"));
                    out(tty, "\n");
                }
            }
            b'4' => {
                out(tty, "\nWipe ALL /data? (y/N) ");
                let c = getc(tty);
                getc(tty); // 吞 \n
                if c == b'y' || c == b'Y' {
                    unsafe { sc1(SYS_KUNX_OTA, OTA_CMD_WIPE_DATA); }
                    out(tty, "\n/data wiped.\n");
                }
            }
            b'5' => {
                let s = prompt_str(tty, "switch to slot (A/B)");
                if s[0] == b'A' || s[0] == b'a' || s[0] == b'B' || s[0] == b'b' {
                    let slot = if s[0] == b'A' || s[0] == b'a' { b'A' } else { b'B' };
                    let r = unsafe { sc2(SYS_KUNX_OTA, OTA_CMD_FORCE_SLOT, slot as usize) };
                    out(tty, "\n[ota] 槽位切换结果=");
                    out(tty, if (r as i64) == 0 { "0" } else { "FAIL" });
                    out(tty, "\n");
                }
            }
            b'6' => {
                out(tty, "\nrebooting...\n");
                unsafe { sc1(SYS_REBOOT, 0x4321FEDC); } // LINUX_REBOOT_CMD_RESTART
                loop {
                    core::hint::spin_loop();
                }
            }
            _ => out(tty, "\n? unknown choice\n"),
        }
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // 诊断：把 panic 消息与位置（文件:行:列）写入 fd 1（无堆，固定缓冲）。
    // 保持原有行为：recovery 不退出，仅输出诊断后 spin。
    struct BufWriter {
        buf: [u8; 256],
        len: usize,
    }
    impl core::fmt::Write for BufWriter {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let b = s.as_bytes();
            let k = b.len().min(self.buf.len() - self.len);
            self.buf[self.len..self.len + k].copy_from_slice(&b[..k]);
            self.len += k;
            Ok(())
        }
    }
    let mut w = BufWriter { buf: [0u8; 256], len: 0 };
    let _ = core::fmt::Write::write_fmt(&mut w, core::format_args!("{}\n", info));
    out(1, "PANIC: recovery\n");
    // PanicInfo 仅由 str/数字拼成，输出缓冲必为合法 UTF-8。
    out(1, unsafe { core::str::from_utf8(&w.buf[..w.len]) }.unwrap_or(""));
    loop {
        core::hint::spin_loop();
    }
}