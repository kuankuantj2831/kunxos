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
    /// memory 节点父级 `#address-cells` / `#size-cells`（决定 reg 的 cell 布局）。
    mem_ac: u32,
    mem_sc: u32,
    /// 父级 cell 数是否实际从 DTB 读到（未读到则回退启发式）。
    mem_cells_known: bool,
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

/// 读取绝对地址 `addr` 处 C 字符串的长度（不含 NUL），最多扫描 `max` 字节
/// （防止损坏/恶意 DTB 中缺失 NUL 导致无界读取）。
unsafe fn cstr_len(addr: usize, max: usize) -> usize {
    let mut n = 0usize;
    while n < max && *((addr + n) as *const u8) != 0 {
        n += 1;
    }
    n
}

/// 判断绝对地址 `addr` 处的 C 字符串是否等于 `s`（最多扫描 `max` 字节）。
unsafe fn cstr_eq(addr: usize, s: &[u8], max: usize) -> bool {
    if s.len() >= max {
        return false;
    }
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
/// `parent_ac` / `parent_sc` / `parent_cells_known`：本节点父级声明的
/// `#address-cells` / `#size-cells`（决定本节点 `reg` 的 cell 布局）。
unsafe fn walk_node(
    hdr: &Hdr,
    mut p: usize,
    ctx: &mut Ctx,
    depth: usize,
    parent_ac: u32,
    parent_sc: u32,
    parent_cells_known: bool,
) -> usize {
    if depth > MAX_DEPTH {
        return p; // 深度超限：停止向下解析（节点其余 token 按非递归逻辑跳过）
    }
    let struct_end = hdr.struct_base + hdr.size_struct;
    let strings_end = hdr.strings_base + hdr.size_strings;
    let mut node_reg_value = 0usize;
    let mut node_reg_len = 0usize;
    let mut node_is_memory = false;
    // 本节点给子节点声明的 cell 数（ePAPR 默认 2/1；未声明则视为未知）
    let mut child_ac = 2u32;
    let mut child_sc = 1u32;
    let mut child_cells_known = false;

    loop {
        // 结构块边界检查：读出越过块尾即损坏/恶意 DTB，停止遍历
        if p + 4 > struct_end {
            return p;
        }
        let t = read_be32(p);
        match t {
            FDT_BEGIN_NODE => {
                let name_off = p + 4;
                let name_len = cstr_len(name_off, struct_end - name_off);
                if cstr_eq(name_off, b"chosen", struct_end - name_off) && ctx.chosen_begin == 0 {
                    ctx.chosen_begin = p;
                    ctx.chosen_name_len = name_len;
                }
                let child = align4(name_off + name_len + 1);
                p = walk_node(hdr, child, ctx, depth + 1, child_ac, child_sc, child_cells_known);
            }
            FDT_END_NODE => {
                if node_is_memory && node_reg_len != 0 {
                    ctx.mem_reg_value = node_reg_value;
                    ctx.mem_reg_len = node_reg_len;
                    ctx.mem_ac = parent_ac;
                    ctx.mem_sc = parent_sc;
                    ctx.mem_cells_known = parent_cells_known;
                }
                return p + 4;
            }
            FDT_PROP => {
                if p + 12 > struct_end {
                    return p;
                }
                let len = read_be32(p + 4) as usize;
                let nameoff = read_be32(p + 8) as usize;
                // 字符串块边界检查：nameoff 越界即损坏/恶意 DTB，停止遍历
                if nameoff >= hdr.size_strings {
                    return p;
                }
                let value_off = p + 12;
                // ★ 值区边界检查：声称 len 字节的属性值必须完整落在 struct
                // 块内 —— 此前不校验，DTB 填满 scratch 时 mem_prefix/mem_reg
                // 补丁会越出 struct 块写穿 .bss 相邻静态变量。
                if value_off + len > struct_end {
                    return p;
                }
                let prop_name = hdr.strings_base + nameoff;
                let name_max = strings_end - prop_name;
                if cstr_eq(prop_name, b"device_type", name_max) && mem_prefix(value_off, b"memory") {
                    node_is_memory = true;
                }
                if cstr_eq(prop_name, b"reg", name_max) {
                    node_reg_value = value_off;
                    node_reg_len = len;
                }
                if cstr_eq(prop_name, b"#address-cells", name_max) {
                    child_ac = read_be32(value_off);
                    child_cells_known = true;
                }
                if cstr_eq(prop_name, b"#size-cells", name_max) {
                    child_sc = read_be32(value_off);
                    child_cells_known = true;
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
/// reg 的 cell 布局由父节点 `#address-cells` / `#size-cells` 决定（size 字段
/// 位于 reg 值偏移 ac×4 处，宽 sc 个 cell）。遍历时已把 memory 节点父级的
/// cell 数记入 ctx；父级声明未知时回退到按 reg 长度的常用组合启发式
/// （16 字节兼容 (2,2)/(3,1)/(1,3)）。
unsafe fn patch_memory(ctx: &Ctx, mem_size: u64) -> bool {
    if ctx.mem_reg_len == 0 || ctx.mem_reg_value == 0 {
        return false;
    }
    // 关键修复：仅当 DTB 内存节点缺失/大小为 0 时才写入默认值。
    // 此前无条件覆写——QEMU -m 512M 的 DTB 被硬编码 128MB 覆盖，内核在
    // bootloader 路径被限制到 128MB（浪费 384MB）。DTB 已有有效内存大小时
    // 保留原值（真实硬件/QEMU 的 DTB 均带正确大小）。
    // 父级 cell 数已知且与 reg 长度吻合：按 (ac, sc) 精确定位 size 字段。
    if ctx.mem_cells_known {
        let ac = ctx.mem_ac as usize;
        let sc = ctx.mem_sc as usize;
        if ac * 4 + sc * 4 == ctx.mem_reg_len && (sc == 1 || sc == 2) {
            let size_off = ctx.mem_reg_value + ac * 4;
            let cur = if sc == 1 {
                read_be32(size_off) as u64
            } else {
                ((read_be32(size_off) as u64) << 32) | read_be32(size_off + 4) as u64
            };
            if cur != 0 {
                return true;
            }
            if sc == 1 {
                write_be32(size_off, mem_size as u32);
            } else {
                write_be32(size_off, (mem_size >> 32) as u32);
                write_be32(size_off + 4, mem_size as u32);
            }
            return true;
        }
    }
    // 启发式回退（父级 cell 数未知，或 (sc, ac) 组合不在支持范围内）：
    // - 16 字节：除 (2,2) 外兼容 (3,1)（size 在 +12）与 (1,3)（size 在 +4/+8/+12）
    // - 12 字节：1 地址 cell + 2 大小 cell，大小写在 +4/+8（不能按 8 字节档处理，
    //   否则会把内存大小写进地址 cell 的高 32 位）
    // - 8 字节：1 地址 cell + 1 大小 cell，大小写在 +4
    match ctx.mem_reg_len {
        16 => {
            // (2,2)：size 在 +8/+12
            let cur = ((read_be32(ctx.mem_reg_value + 8) as u64) << 32)
                | read_be32(ctx.mem_reg_value + 12) as u64;
            if cur != 0 {
                return true;
            }
            // (3,1)：size 在 +12（1 cell）
            if read_be32(ctx.mem_reg_value + 12) as u64 != 0 {
                return true;
            }
            // (1,3)：size 在 +4/+8/+12（低 64 位落在 +4/+8）
            let cur3 = ((read_be32(ctx.mem_reg_value + 4) as u64) << 32)
                | read_be32(ctx.mem_reg_value + 8) as u64;
            if cur3 != 0 {
                return true;
            }
            // 全 0：父级未声明但启发式得知 (1,3) 时写低 64 位到 +4/+8，否则按 (2,2)
            if ctx.mem_cells_known && ctx.mem_sc == 3 {
                write_be32(ctx.mem_reg_value + 4, (mem_size >> 32) as u32);
                write_be32(ctx.mem_reg_value + 8, mem_size as u32);
            } else {
                write_be32(ctx.mem_reg_value + 8, (mem_size >> 32) as u32);
                write_be32(ctx.mem_reg_value + 12, mem_size as u32);
            }
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

/// 在 chosen 节点中插入多个属性 `props`（(名字, 值) 列表；值不含 NUL，
/// 字符串属性由调用方自行决定是否包含结尾 NUL）。必要时整体搬移结构块与字符串块。
/// 所有地址均为绝对地址（scratch 缓冲内）。
unsafe fn insert_props(hdr: &mut Hdr, ctx: &Ctx, props: &[(&[u8], &[u8])]) -> bool {
    if ctx.chosen_begin == 0 {
        return false;
    }
    // 本函数假设字符串块紧跟结构块末尾（content_end == strings_base +
    // size_strings，QEMU/dtc/mkdisk 生成的布局均如此）：新属性名追加在
    // content_end + struct_added 处，nameoff 相对搬移后的旧字符串基址。
    // FDT 规范并不强制两块顺序——非该布局时追加的名字与 nameoff 脱节，
    // 内核会把垃圾字节当属性名读，注入静默失效。拒绝而非注入。
    if hdr.strings_base + hdr.size_strings != hdr.content_end {
        return false;
    }
    // 插入点：chosen 节点名字之后
    let name_off = ctx.chosen_begin + 4;
    let insert_off = align4(name_off + ctx.chosen_name_len + 1);

    // 结构块新增量：每个属性 = 12(prop 头) + align4(value_len)；
    // 字符串块新增量 = align4(name_len+1)，追加在搬移后的字符串块末尾。
    // 搬移只让出 struct_added——若按 total(=struct_added+str_added) 搬移，
    // 结构块中间会留下 str_added 字节空洞（写入头 size_struct 后成为垃圾区）。
    let mut struct_added = 0usize;
    let mut str_added = 0usize;
    for (name, value) in props {
        struct_added += 12 + align4(value.len());
        str_added += align4(name.len() + 1);
    }
    let content_end = hdr.content_end;
    let scratch_end = SCRATCH.as_ptr() as usize + SCRATCH_SIZE;
    // 边界检查：结构块搬移 + 追加字符串都必须在 scratch 内
    if content_end + struct_added + str_added > scratch_end {
        return false;
    }

    // 从后向前搬移，避免覆盖
    for i in (insert_off..content_end).rev() {
        *((i + struct_added) as *mut u8) = *(i as *const u8);
    }

    let mut p = insert_off;
    let mut strings_p = content_end + struct_added;
    // 字符串块被整体搬移 struct_added；新基址 = 旧基址 + struct_added。
    // nameoff 必须相对新基址。
    let new_strings_base = hdr.strings_base + struct_added;
    for (name, value) in props {
        // 属性头
        write_be32(p, FDT_PROP);
        write_be32(p + 4, value.len() as u32);
        write_be32(p + 8, (strings_p - new_strings_base) as u32);
        let vp = p + 12;
        for (j, &b) in value.iter().enumerate() {
            *((vp + j) as *mut u8) = b;
        }
        // 字符串追加
        for (j, &b) in name.iter().enumerate() {
            *((strings_p + j) as *mut u8) = b;
        }
        *((strings_p + name.len()) as *mut u8) = 0;
        p += 12 + align4(value.len());
        strings_p += align4(name.len() + 1);
    }

    // 更新头
    hdr.size_struct += struct_added;
    hdr.strings_base += struct_added;
    hdr.size_strings += str_added;
    hdr.content_end = strings_p;
    true
}

/// 失败诊断转储（独立 noinline 函数，尽量不影响主流程代码生成）。
#[inline(never)]
unsafe fn dump_fixup_fail(base: usize, dtb_addr: usize, ctx: &Ctx, mem_ok: bool, props_ok: bool) {
    crate::uart::send_str("[dtb-dbg] base=");
    crate::uart::print_hex(base as u64);
    crate::uart::send_str(" dtb=");
    crate::uart::print_hex(dtb_addr as u64);
    crate::uart::send_str("\n[dtb-dbg] scratch[64..96]:");
    for w in 0..8 {
        crate::uart::send_str(" ");
        crate::uart::print_hex(read_be32(base + 64 + w * 4) as u64);
    }
    crate::uart::send_str("\n[dtb-dbg] dtb_src[64..96] :");
    for w in 0..8 {
        crate::uart::send_str(" ");
        crate::uart::print_hex(read_be32(dtb_addr + 64 + w * 4) as u64);
    }
    crate::uart::send_str("\n[dtb-dbg] mem_len=");
    crate::uart::print_hex(ctx.mem_reg_len as u64);
    crate::uart::send_str(" chosen=");
    crate::uart::print_hex(ctx.chosen_begin as u64);
    crate::uart::send_str(" mem_ok=");
    crate::uart::print_dec(mem_ok as u64);
    crate::uart::send_str(" props_ok=");
    crate::uart::print_dec(props_ok as u64);
    crate::uart::send_str(" root_byte=");
    crate::uart::print_hex(*((base + 68) as *const u8) as u64);
    crate::uart::send_str("\n");
}

/// 修复 DTB：修改内存大小并追加安全属性，结果写回 `dtb_addr`。
/// 成功返回 `Some(修复后 DTB 总长)`（属性注入会增长 DTB，调用方须以该长度
/// 做跳转前的缓存维护，否则增长尾部可能滞留脏缓存行），失败返回 `None`。
pub fn fixup(dtb_addr: usize, mem_size: u64, props: &[(&[u8], &[u8])]) -> Option<usize> {
    unsafe {
        if read_be32(dtb_addr) != FDT_MAGIC {
            crate::uart::send_str("[dtb] DTB 修正失败：魔术字无效\n");
            return None;
        }
        let off_struct = read_be32(dtb_addr + 8) as usize;
        let off_strings = read_be32(dtb_addr + 12) as usize;
        let size_strings = read_be32(dtb_addr + 32) as usize;
        let size_struct = read_be32(dtb_addr + 36) as usize;
        let content_end = core::cmp::max(off_struct + size_struct, off_strings + size_strings);
        if content_end > SCRATCH_SIZE {
            crate::uart::send_str("[dtb] DTB 修正失败：dtb 过大\n");
            return None;
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
            mem_ac: 0,
            mem_sc: 0,
            mem_cells_known: false,
            chosen_begin: 0,
            chosen_name_len: 0,
        };
        let root_name_off = hdr.struct_base + 4;
        // 防御 size_struct < 4 的畸形/截断 DTB：release 下 `size_struct - 4`
        // 回绕成巨大值，cstr_len 将无界扫描越出 scratch
        if hdr.size_struct < 4 {
            crate::uart::send_str("[dtb] DTB 修正失败：struct 区过小\n");
            return None;
        }
        let root_content =
            align4(root_name_off + cstr_len(root_name_off, hdr.size_struct - 4) + 1);
        walk_node(&hdr, root_content, &mut ctx, 0, 0, 0, false);

        let mem_ok = patch_memory(&ctx, mem_size);
        let props_ok = insert_props(&mut hdr, &ctx, props);
        if !mem_ok || !props_ok {
            dump_fixup_fail(base, dtb_addr, &ctx, mem_ok, props_ok);
        }
        if !mem_ok {
            crate::uart::send_str("[dtb] 警告：/memory reg 未修补\n");
        }
        if !props_ok {
            crate::uart::send_str("[dtb] 警告：chosen 属性未注入\n");
            return None;
        }

        // 更新头字段
        write_be32(base + 4, (hdr.content_end - base) as u32); // totalsize
        write_be32(base + 8, (hdr.struct_base - base) as u32); // off_dt_struct
        write_be32(base + 12, (hdr.strings_base - base) as u32); // off_dt_strings
        write_be32(base + 32, hdr.size_strings as u32);
        write_be32(base + 36, hdr.size_struct as u32);

        // 写回目标地址
        ptr::copy_nonoverlapping(base as *const u8, dtb_addr as *mut u8, hdr.content_end - base);
        Some(hdr.content_end - base)
    }
}
