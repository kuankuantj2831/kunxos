//! 块存储驱动（MMC/SD 抽象层）。
//!
//! QEMU virt：使用 virtio-blk（virtio-mmio 传输，legacy 模式）读取磁盘镜像。
//! Raspberry Pi 4：EMMC2 SD 卡骨架（仅编译通过）。

/// 块大小（字节）。
pub const BLOCK_SIZE: usize = 512;

#[cfg(not(feature = "pi4"))]
mod virtio {
    use core::arch::asm;
    use core::ptr::{read_volatile, write_volatile};
    use super::BLOCK_SIZE;

    // virtio-mmio 传输区域（QEMU virt：基址 0x0a000000，每设备 0x200 字节，共 32 个槽）
    const MMIO_BASE: usize = 0x0a00_0000;
    const MMIO_STRIDE: usize = 0x200;
    const MMIO_SLOTS: usize = 32;

    // virtio-mmio 寄存器偏移
    const MMIO_MAGIC: usize = 0x000;
    const MMIO_VERSION: usize = 0x004;
    const MMIO_DEVICE_ID: usize = 0x008;
    const MMIO_GUEST_PAGE_SIZE: usize = 0x028;
    const MMIO_QUEUE_SEL: usize = 0x030;
    const MMIO_QUEUE_NUM_MAX: usize = 0x034;
    const MMIO_QUEUE_NUM: usize = 0x038;
    const MMIO_QUEUE_ALIGN: usize = 0x03c;
    const MMIO_QUEUE_PFN: usize = 0x040;
    const MMIO_QUEUE_NOTIFY: usize = 0x050;
    const MMIO_STATUS: usize = 0x070;

    const VIRTIO_MAGIC: u32 = 0x7472_6976; // "virt"
    const VIRTIO_ID_BLOCK: u32 = 2;

    // 设备状态位
    const STATUS_ACKNOWLEDGE: u32 = 1;
    const STATUS_DRIVER: u32 = 2;
    const STATUS_DRIVER_OK: u32 = 4;
    const STATUS_FEATURES_OK: u32 = 8;

    // virtio-blk 请求类型：读
    const VIRTIO_BLK_T_IN: u32 = 0;

    // 描述符标志
    const DESC_F_NEXT: u16 = 1;
    const DESC_F_WRITE: u16 = 2;

    const QUEUE_SIZE: usize = 16;
    const GUEST_PAGE_SIZE: u32 = 4096;

    /// 找到的 virtio-blk 设备 MMIO 基地址。
    static mut VIRTIO_BASE: usize = 0;

    /// virtqueue 缓冲（4096 对齐）：描述符表 + available ring + used ring。
    #[repr(C, align(4096))]
    struct Aligned([u8; 8192]);
    static mut VQ: Aligned = Aligned([0; 8192]);

    /// virtio-blk 输出头（type + ioprio + sector，16 字节，小端）。
    static mut OUT_HDR: [u8; 16] = [0; 16];
    /// 请求状态字节（设备写入，0 表示成功）。
    static mut STATUS_BYTE: u8 = 0;

    #[inline]
    unsafe fn read32(addr: usize) -> u32 {
        read_volatile(addr as *const u32)
    }

    #[inline]
    unsafe fn write32(addr: usize, val: u32) {
        write_volatile(addr as *mut u32, val)
    }

    #[inline]
    unsafe fn reg_read(off: usize) -> u32 {
        read32(VIRTIO_BASE + off)
    }

    #[inline]
    unsafe fn reg_write(off: usize, val: u32) {
        write32(VIRTIO_BASE + off, val)
    }

    #[inline]
    unsafe fn write_le16(p: *mut u8, v: u16) {
        core::ptr::write_unaligned(p as *mut u16, v.to_le());
    }

    #[inline]
    unsafe fn write_le32(p: *mut u8, v: u32) {
        core::ptr::write_unaligned(p as *mut u32, v.to_le());
    }

    #[inline]
    unsafe fn write_le64(p: *mut u8, v: u64) {
        core::ptr::write_unaligned(p as *mut u64, v.to_le());
    }

    #[inline]
    unsafe fn read_le16(p: *const u8) -> u16 {
        u16::from_le(core::ptr::read_unaligned(p as *const u16))
    }

    #[inline]
    fn mb() {
        unsafe { asm!("dmb sy", options(nomem, nostack)) };
    }

    /// 初始化 virtio-blk 设备，返回是否成功。
    fn init_device() -> bool {
        unsafe {
            // 仅支持 legacy（version 1）传输
            if reg_read(MMIO_VERSION) != 1 {
                return false;
            }
            // 复位
            reg_write(MMIO_STATUS, 0);
            // ACKNOWLEDGE
            reg_write(MMIO_STATUS, STATUS_ACKNOWLEDGE);
            // ACKNOWLEDGE | DRIVER
            reg_write(MMIO_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);
            // legacy：guest 页大小
            reg_write(MMIO_GUEST_PAGE_SIZE, GUEST_PAGE_SIZE);
            // 队列 0
            reg_write(MMIO_QUEUE_SEL, 0);
            let num_max = reg_read(MMIO_QUEUE_NUM_MAX);
            let num = if num_max > QUEUE_SIZE as u32 {
                QUEUE_SIZE as u32
            } else {
                num_max
            };
            if num == 0 {
                return false;
            }
            reg_write(MMIO_QUEUE_NUM, num);
            reg_write(MMIO_QUEUE_ALIGN, GUEST_PAGE_SIZE);
            // 队列物理页帧号
            let q = VQ.0.as_ptr() as usize;
            reg_write(MMIO_QUEUE_PFN, (q >> 12) as u32);
            // FEATURES_OK
            reg_write(
                MMIO_STATUS,
                STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK,
            );
            // DRIVER_OK
            reg_write(
                MMIO_STATUS,
                STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK,
            );
        }
        true
    }

    /// 扫描 virtio-mmio 区域寻找块设备并初始化。
    pub fn init() -> bool {
        for slot in 0..MMIO_SLOTS {
            let base = MMIO_BASE + slot * MMIO_STRIDE;
            let magic = unsafe { read32(base + MMIO_MAGIC) };
            if magic != VIRTIO_MAGIC {
                continue;
            }
            let dev_id = unsafe { read32(base + MMIO_DEVICE_ID) };
            if dev_id != VIRTIO_ID_BLOCK {
                continue;
            }
            unsafe {
                VIRTIO_BASE = base;
            }
            return init_device();
        }
        false
    }

    /// 读取单个 512 字节块到 `dst`。
    pub fn read_block(lba: u64, dst: *mut u8) -> bool {
        unsafe {
            if VIRTIO_BASE == 0 {
                return false;
            }
            let desc = VQ.0.as_mut_ptr();
            let avail = desc.add(256);
            let used = desc.add(4096);

            // 填充 virtio-blk 输出头
            write_le32(OUT_HDR.as_mut_ptr(), VIRTIO_BLK_T_IN);
            write_le32(OUT_HDR.as_mut_ptr().add(4), 0); // ioprio
            write_le64(OUT_HDR.as_mut_ptr().add(8), lba);

            // 描述符 0：输出头（只读）
            write_le64(desc, OUT_HDR.as_ptr() as u64);
            write_le32(desc.add(8), 16);
            write_le16(desc.add(12), DESC_F_NEXT);
            write_le16(desc.add(14), 1);

            // 描述符 1：数据缓冲（设备写入）
            write_le64(desc.add(16), dst as u64);
            write_le32(desc.add(24), BLOCK_SIZE as u32);
            write_le16(desc.add(28), DESC_F_NEXT | DESC_F_WRITE);
            write_le16(desc.add(30), 2);

            // 描述符 2：状态字节（设备写入）
            write_le64(desc.add(32), (&mut STATUS_BYTE as *mut u8) as u64);
            write_le32(desc.add(40), 1);
            write_le16(desc.add(44), DESC_F_WRITE);
            write_le16(desc.add(46), 0);

            // available ring：追加描述符 0
            let idx = read_le16(avail.add(2)) as usize;
            let old_used = read_le16(used.add(2)) as usize;
            write_le16(avail.add(4 + (idx % QUEUE_SIZE) * 2), 0);
            write_le16(avail.add(2), (idx as u16).wrapping_add(1));

            // 内存屏障 + 通知设备
            mb();
            reg_write(MMIO_QUEUE_NOTIFY, 0);

            // 轮询 used ring 直到设备完成（带超时，防止损坏设备/总线挂起导致 bootloader 死循环）
            let mut tries = 0usize;
            while read_le16(used.add(2)) as usize == old_used {
                tries += 1;
                if tries > 100_000_000 {
                    return false;
                }
                core::hint::spin_loop();
            }
            mb();

            STATUS_BYTE == 0
        }
    }
}

#[cfg(feature = "pi4")]
mod virtio {
    // Pi 4 EMMC2 SD 卡骨架：接口占位，仅保证编译通过。
    pub fn init() -> bool {
        false
    }

    pub fn read_block(_lba: u64, _dst: *mut u8) -> bool {
        false
    }
}

/// 初始化块存储设备。
pub fn init() -> bool {
    virtio::init()
}

/// 读取单个块（512 字节）。
pub fn read_block(lba: u64, dst: *mut u8) -> bool {
    virtio::read_block(lba, dst)
}

/// 从字节偏移 `offset` 开始读取 `len` 字节到 `dst`（自动处理非对齐边界）。
pub fn read_bytes(offset: u64, dst: *mut u8, len: usize) -> bool {
    let mut cur = offset;
    let mut done = 0usize;
    let mut tmp = [0u8; BLOCK_SIZE];

    // 非对齐头
    if (cur as usize) % BLOCK_SIZE != 0 {
        let lba = cur / BLOCK_SIZE as u64;
        if !read_block(lba, tmp.as_mut_ptr()) {
            return false;
        }
        let off = (cur as usize) % BLOCK_SIZE;
        let n = core::cmp::min(BLOCK_SIZE - off, len);
        unsafe {
            core::ptr::copy_nonoverlapping(tmp.as_ptr().add(off), dst.add(done), n);
        }
        done += n;
        cur += n as u64;
    }

    // 对齐的整块
    while len - done >= BLOCK_SIZE {
        let lba = cur / BLOCK_SIZE as u64;
        if !read_block(lba, unsafe { dst.add(done) }) {
            return false;
        }
        done += BLOCK_SIZE;
        cur += BLOCK_SIZE as u64;
    }

    // 尾部
    if done < len {
        let lba = cur / BLOCK_SIZE as u64;
        if !read_block(lba, tmp.as_mut_ptr()) {
            return false;
        }
        let n = len - done;
        unsafe {
            core::ptr::copy_nonoverlapping(tmp.as_ptr(), dst.add(done), n);
        }
    }
    true
}
