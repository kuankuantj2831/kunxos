//! Flattened Device Tree (FDT / DTB) 修复。
//!
//! 1. 修复 `/memory` 节点的 `reg` 属性（把内存大小改为实际值）。
//! 2. 向 `/chosen` 节点插入 `bootargs` 属性。
//!
//! 全部操作在静态 scratch 缓冲中进行（绝对地址），完成后写回目标地址。

use core::ptr;

const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;

/// scratch 缓冲大小（远大于精简后的 DTB）。
const SCRATCH_SIZE: usize = 128 * 1024;
static mut SCRATCH: [u8; SCRATCH_SIZE] = [0; SCRATCH_SIZE];

/// 解析后的 FDT 头（绝对地址）。
struct Hdr {
    struct_base: usize,
    strings_base: usize,
    size_strings: usize,
    size_struct: usize,
    content_end: usize,
}

/// 遍历时记录的定位信息。
struct Ctx {
    mem_reg_value: usize,
    mem_reg_len: usize,
    chosen_begin: usize,
    chosen_name_len: usize,
}

#[inline]
fn align4(v: usize) -> usize {
    (v + 3) & !3
}

/// 读取绝对地址 `addr` 处的大端 u32（容忍非对齐）。
#[inline]
unsafe fn read_be32(addr: usize) -> u32 {
    u32::from_be(ptr::read_unaligned(addr as *const u32))
}

/// 向绝对地址 `addr` 写入大端 u32。
#[inline]
unsafe fn write_be32(addr: usize, v: u32) {
    ptr::write_unaligned(addr as *mut u32, v.to_be());
}

/// 读取绝对地址 `addr` 处 C 字符串的长度（不含 NUL）。
unsafe fn cstr_len(addr: usize) -> usize {
    let mut n = 0usize;
    while *((addr + n) as *const u8) != 0 {
        n += 1;
    }
    n
}

/// 判断绝对地址 `addr` 处的 C 字符串是否等于 `s`。
unsafe fn cstr_eq(addr: usize, s: &[u8]) -> bool {
    for (i, &b) in s.iter().enumerate() {
        if *((addr + i) as *const u8) != b {
            return false;
        }
    }
    *((addr + s.len()) as *const u8) == 0
}

/// 判断绝对地址 `addr` 处的前 `s.len()` 字节是否等于 `s`。
unsafe fn mem_prefix(addr: usize, s: &[u8]) -> bool {
    for (i, &b) in s.iter().enumerate() {
        if *((addr + i) as *const u8) != b {
            return false;
        }
    }
    true
}

/// 最大节点嵌套深度（防恶意/损坏 DTB 构造深嵌套 BEGIN_NODE 导致
/// bootloader 栈溢出）。
const MAX_DEPTH: usize = 16;

/// 递归遍历结构块，记录 memory 节点的 `reg` 与 chosen 节点位置。
/// `p` 指向某节点内部第一个 token（绝对地址），返回该节点 END_NODE 之后的绝对地址。
unsafe fn walk_node(hdr: &Hdr, mut p: usize, ctx: &mut Ctx, depth: usize) -> usize {
    if depth > MAX_DEPTH {
        return p; // 深度超限：停止向下解析（节点其余 token 按非递归逻辑跳过）
    }
    let mut node_reg_value = 0usize;
    let mut node_reg_len = 0usize;
    let mut node_is_memory = false;

    loop {
        let t = read_be32(p);
        match t {
            FDT_BEGIN_NODE => {
                let name_off = p + 4;
                let name_len = cstr_len(name_off);
                if cstr_eq(name_off, b"chosen") && ctx.chosen_begin == 0 {
                    ctx.chosen_begin = p;
                    ctx.chosen_name_len = name_len;
                }
                let child = align4(name_off + name_len + 1);
                p = walk_node(hdr, child, ctx, depth + 1);
            }
            FDT_END_NODE => {
                if node_is_memory && node_reg_len != 0 {
                    ctx.mem_reg_value = node_reg_value;
                    ctx.mem_reg_len = node_reg_len;
                }
                return p + 4;
            }
            FDT_PROP => {
                let len = read_be32(p + 4) as usize;
                let nameoff = read_be32(p + 8) as usize;
                let value_off = p + 12;
                let prop_name = hdr.strings_base + nameoff;
                if cstr_eq(prop_name, b"device_type") && mem_prefix(value_off, b"memory") {
                    node_is_memory = true;
                }
                if cstr_eq(prop_name, b"reg") {
                    node_reg_value = value_off;
                    node_reg_len = len;
                }
                p = align4(value_off + len);
            }
            FDT_NOP => p += 4,
            FDT_END => return p + 4,
            _ => return p + 4,
        }
    }
}

/// 原地修复 `/memory` 的 `reg` 大小。
///
/// 按常见的 #address-cells/#size-cells 组合分档处理（reg 长度即 cell 数 × 4）：
/// - 16 字节：2 地址 cell + 2 大小 cell，大小写在 +8/+12
/// - 12 字节：1 地址 cell + 2 大小 cell，大小写在 +4/+8（不能按 8 字节档处理，
///   否则会把内存大小写进地址 cell 的高 32 位）
/// - 8 字节：1 地址 cell + 1 大小 cell，大小写在 +4
unsafe fn patch_memory(ctx: &Ctx, mem_size: u64) -> bool {
    if ctx.mem_reg_len == 0 || ctx.mem_reg_value == 0 {
        return false;
    }
    // 关键修复：仅当 DTB 内存节点缺失/大小为 0 时才写入默认值。
    // 此前无条件覆写——QEMU -m 512M 的 DTB 被硬编码 128MB 覆盖，内核在
    // bootloader 路径被限制到 128MB（浪费 384MB）。DTB 已有有效内存大小时
    // 保留原值（真实硬件/QEMU 的 DTB 均带正确大小）。
    match ctx.mem_reg_len {
        16 => {
            let cur = ((read_be32(ctx.mem_reg_value + 8) as u64) << 32)
                | read_be32(ctx.mem_reg_value + 12) as u64;
            if cur != 0 {
                return true;
            }
            write_be32(ctx.mem_reg_value + 8, (mem_size >> 32) as u32);
            write_be32(ctx.mem_reg_value + 12, mem_size as u32);
            true
        }
        12 => {
            let cur = ((read_be32(ctx.mem_reg_value + 4) as u64) << 32)
                | read_be32(ctx.mem_reg_value + 8) as u64;
            if cur != 0 {
                return true;
            }
            write_be32(ctx.mem_reg_value + 4, (mem_size >> 32) as u32);
            write_be32(ctx.mem_reg_value + 8, mem_size as u32);
            true
        }
        8 => {
            if read_be32(ctx.mem_reg_value + 4) as u64 != 0 {
                return true;
            }
            write_be32(ctx.mem_reg_value + 4, mem_size as u32);
            true
        }
        _ => false,
    }
}

/// 在 chosen 节点中插入 `bootargs` 属性（必要时整体搬移结构块与字符串块）。
/// 所有地址均为绝对地址（scratch 缓冲内）。
unsafe fn insert_bootargs(hdr: &mut Hdr, ctx: &Ctx, bootargs: &[u8]) -> bool {
    if ctx.chosen_begin == 0 {
        return false;
    }
    // 插入点：chosen 节点名字之后
    let name_off = ctx.chosen_begin + 4;
    let insert_off = align4(name_off + ctx.chosen_name_len + 1);

    let value_len = bootargs.len() + 1; // 含 NUL
    let prop_size = 12 + align4(value_len);
    let content_end = hdr.content_end;

    // 越界检查：搬移上移 prop_size 并在尾部追加 "bootargs\0"（9 字节），
    // 最大写入地址必须仍在 scratch 缓冲内，否则静默破坏相邻静态变量。
    let scratch_end = SCRATCH.as_ptr() as usize + SCRATCH_SIZE;
    if content_end + prop_size + 9 > scratch_end {
        return false;
    }

    // 从后向前搬移，避免覆盖
    for i in (insert_off..content_end).rev() {
        *((i + prop_size) as *mut u8) = *(i as *const u8);
    }

    // 写 FDT_PROP
    write_be32(insert_off, FDT_PROP);
    write_be32(insert_off + 4, value_len as u32);
    write_be32(insert_off + 8, hdr.size_strings as u32); // nameoff 指向追加的字符串
    for (j, &b) in bootargs.iter().enumerate() {
        *((insert_off + 12 + j) as *mut u8) = b;
    }
    *((insert_off + 12 + bootargs.len()) as *mut u8) = 0;

    // 追加 "bootargs\0" 到字符串块末尾
    let strings_append = content_end + prop_size;
    for (j, &b) in b"bootargs".iter().enumerate() {
        *((strings_append + j) as *mut u8) = b;
    }
    *((strings_append + 8) as *mut u8) = 0;

    // 更新头
    hdr.size_struct += prop_size;
    hdr.strings_base += prop_size;
    hdr.size_strings += 9; // "bootargs\0"
    hdr.content_end = strings_append + 9;
    true
}

/// 修复 DTB：修改内存大小并添加 bootargs，结果写回 `dtb_addr`。
pub fn fixup(dtb_addr: usize, mem_size: u64) {
    unsafe {
        if read_be32(dtb_addr) != FDT_MAGIC {
            return;
        }
        let off_struct = read_be32(dtb_addr + 8) as usize;
        let off_strings = read_be32(dtb_addr + 12) as usize;
        let size_strings = read_be32(dtb_addr + 32) as usize;
        let size_struct = read_be32(dtb_addr + 36) as usize;
        let content_end = core::cmp::max(off_struct + size_struct, off_strings + size_strings);
        if content_end > SCRATCH_SIZE {
            return;
        }

        let base = SCRATCH.as_ptr() as usize;

        // 复制 DTB 内容到 scratch
        ptr::copy_nonoverlapping(dtb_addr as *const u8, base as *mut u8, content_end);

        let mut hdr = Hdr {
            struct_base: base + off_struct,
            strings_base: base + off_strings,
            size_strings,
            size_struct,
            content_end: base + content_end,
        };

        // 遍历记录 memory / chosen
        let mut ctx = Ctx {
            mem_reg_value: 0,
            mem_reg_len: 0,
            chosen_begin: 0,
            chosen_name_len: 0,
        };
        let root_name_off = hdr.struct_base + 4;
        let root_content = align4(root_name_off + cstr_len(root_name_off) + 1);
        walk_node(&hdr, root_content, &mut ctx, 0);

        patch_memory(&ctx, mem_size);
        insert_bootargs(&mut hdr, &ctx, b"console=ttyAMA0");

        // 更新头字段
        write_be32(base + 4, (hdr.content_end - base) as u32); // totalsize
        write_be32(base + 8, (hdr.struct_base - base) as u32); // off_dt_struct
        write_be32(base + 12, (hdr.strings_base - base) as u32); // off_dt_strings
        write_be32(base + 32, hdr.size_strings as u32);
        write_be32(base + 36, hdr.size_struct as u32);

        // 写回目标地址
        ptr::copy_nonoverlapping(base as *const u8, dtb_addr as *mut u8, hdr.content_end - base);
    }
}
