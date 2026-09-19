//! Flattened Device Tree (FDT / DTB) 解析器。
//!
//! 解析 QEMU 启动时通过 x0 寄存器传入的扁平设备树，提供按路径或 compatible
//! 字符串查找节点、读取属性（字符串 / u32 / u64 / reg / interrupts）的能力。
//! 所有字段均为大端，按 FDT v17 规范解析。

use core::slice;

/// FDT 魔数（大端 `0xd00dfeed`）
pub const FDT_MAGIC: u32 = 0xd00dfeed;

// 结构块 token 定义
const FDT_BEGIN_NODE: u32 = 0x0000_0001;
const FDT_END_NODE: u32 = 0x0000_0002;
const FDT_PROP: u32 = 0x0000_0003;
const FDT_NOP: u32 = 0x0000_0004;
const FDT_END: u32 = 0x0000_0009;

/// 读取大端 u32（容忍非 4 字节对齐地址）。
#[inline]
unsafe fn read_be32(addr: usize) -> u32 {
    u32::from_be(core::ptr::read_unaligned(addr as *const u32))
}

/// 4 字节向上对齐。
#[inline]
fn align4(v: usize) -> usize {
    (v + 3) & !3
}

/// 解析后的 FDT 视图（内部保存的均为绝对地址）。
#[derive(Clone, Copy)]
pub struct Fdt {
    struct_base: usize,
    /// 结构块结束地址（越界访问按 FDT_END 处理，防止损坏 DTB 造成越界读）。
    struct_end: usize,
    strings_base: usize,
    strings_size: usize,
}

/// 一个设备树节点句柄（指向该节点内部第一个 token 的绝对偏移）。
#[derive(Clone, Copy)]
pub struct Node {
    offset: usize,
}

/// 一个属性：原始值字节。
pub struct Prop<'a> {
    pub value: &'a [u8],
}

impl Fdt {
    /// 从 DTB 基地址解析 FDT 头，返回 Fdt 视图；魔数/布局不符返回 `None`。
    ///
    /// # Safety
    /// `addr` 必须指向有效的 FDT 头。
    pub unsafe fn parse(addr: usize) -> Option<Fdt> {
        if read_be32(addr) != FDT_MAGIC {
            return None;
        }
        let totalsize = read_be32(addr + 4) as usize;
        let off_struct = read_be32(addr + 8) as usize;
        let off_strings = read_be32(addr + 12) as usize;
        // FDT 头布局：off_mem_rsvmap@16、version@20、last_comp@24、boot_cpuid@28、
        // **size_dt_strings@32、size_dt_struct@36**。此前误读 16（off_mem_rsvmap，
        // 典型值 ~48）作为结构块大小，导致 struct_end 落在根节点附近，virtio 等
        // 深层节点全部扫描不到（net/blk 设备检测失效、/data 不持久化）。
        let size_struct = read_be32(addr + 36) as usize;
        let size_strings = read_be32(addr + 32) as usize;
        // 头部各区域边界校验：防损坏 DTB 造成越界解析
        if off_struct.checked_add(size_struct).map_or(true, |e| e > totalsize)
            || off_strings.checked_add(size_strings).map_or(true, |e| e > totalsize)
        {
            return None;
        }
        Some(Fdt {
            struct_base: addr + off_struct,
            struct_end: addr + off_struct + size_struct,
            strings_base: addr + off_strings,
            strings_size: size_strings,
        })
    }

    /// 读取结构块中绝对地址 `p` 处的 token；越界按 `FDT_END` 处理。
    #[inline]
    fn token(&self, p: usize) -> u32 {
        if p + 4 > self.struct_end {
            return FDT_END;
        }
        unsafe { read_be32(p) }
    }

    /// 读取 `p` 处的 C 字符串，返回 (字符串, 下一个 token 的绝对地址)。
    ///
    /// 扫描以结构块结束为界：未找到 NUL 视为损坏，返回空串（不越界读）。
    fn read_cstr(&self, p: usize) -> (&'static str, usize) {
        unsafe {
            let mut len = 0usize;
            while p + len < self.struct_end && *(p as *const u8).add(len) != 0 {
                len += 1;
            }
            if p + len >= self.struct_end {
                // 无终止符：损坏。必须至少前进 4 字节，否则 `skip_subtree`/`search`
                // 对同一地址反复解析同一 token → 深度只增不减的死循环（启动挂死）。
                let adv = 4usize.min(self.struct_end.saturating_sub(p).max(1));
                return ("", p + adv);
            }
            // 安全 UTF-8 校验（DTB 属性名可能含任意字节；from_utf8_unchecked 为 UB）
            let raw = slice::from_raw_parts(p as *const u8, len);
            let s = core::str::from_utf8(raw).unwrap_or("");
            (s, p + align4(len + 1))
        }
    }

    /// 读取字符串块中偏移 `off` 处的字符串；越界返回空串。
    fn string_at(&self, off: usize) -> &'static str {
        if off >= self.strings_size {
            return "";
        }
        unsafe {
            let ptr = (self.strings_base + off) as *const u8;
            let mut len = 0usize;
            while off + len < self.strings_size && *ptr.add(len) != 0 {
                len += 1;
            }
            let raw = slice::from_raw_parts(ptr, len);
            core::str::from_utf8(raw).unwrap_or("")
        }
    }

    /// 属性数据区 `p + 8 + align4(len)` 是否完整落在结构块内。
    #[inline]
    fn prop_data_ok(&self, p: usize, len: usize) -> bool {
        p.checked_add(8)
            .and_then(|e| e.checked_add(align4(len)))
            .map_or(false, |e| e <= self.struct_end)
    }

    /// 跳过 `p` 起始的整棵子树（`p` 指向某节点内部第一个 token）。
    /// 返回该子树 END_NODE 之后的绝对地址。
    fn skip_subtree(&self, mut p: usize) -> usize {
        let mut depth = 1usize;
        loop {
            let t = self.token(p);
            p += 4;
            match t {
                FDT_BEGIN_NODE => {
                    let (_, next) = self.read_cstr(p);
                    p = next;
                    depth += 1;
                }
                FDT_END_NODE => {
                    depth -= 1;
                    if depth == 0 {
                        return p;
                    }
                }
                FDT_PROP => {
                    let len = self.token(p) as usize;
                    if !self.prop_data_ok(p, len) {
                        return p;
                    }
                    p += 8 + align4(len);
                }
                FDT_NOP => {}
                _ => return p,
            }
        }
    }

    /// 在 `p`（某节点内部第一个 token）开始的子树中按相对路径查找节点。
    fn search(&self, mut p: usize, path: &str) -> Option<usize> {
        loop {
            let t = self.token(p);
            p += 4;
            match t {
                FDT_BEGIN_NODE => {
                    let (name, next) = self.read_cstr(p);
                    p = next;
                    if name == path {
                        return Some(p);
                    }
                    // path 以 "name/" 开头则递归进入该子节点
                    if path.len() > name.len()
                        && path.starts_with(name)
                        && path.as_bytes()[name.len()] == b'/'
                    {
                        let rest = &path[name.len() + 1..];
                        if let Some(found) = self.search(p, rest) {
                            return Some(found);
                        }
                    }
                    p = self.skip_subtree(p);
                }
                FDT_END_NODE => return None,
                FDT_PROP => {
                    let len = self.token(p) as usize;
                    if !self.prop_data_ok(p, len) {
                        return None;
                    }
                    p += 8 + align4(len);
                }
                FDT_NOP => {}
                FDT_END => return None,
                _ => return None,
            }
        }
    }

    /// 读取 `p`（节点内部第一个 token）处节点的指定属性（仅本层，不进入子节点）。
    fn prop_at(&self, mut p: usize, name: &str) -> Option<Prop<'static>> {
        loop {
            let t = self.token(p);
            p += 4;
            match t {
                FDT_BEGIN_NODE => {
                    let (_, next) = self.read_cstr(p);
                    p = self.skip_subtree(next);
                }
                FDT_END_NODE => return None,
                FDT_PROP => {
                    let len = self.token(p) as usize;
                    let nameoff = self.token(p + 4) as usize;
                    let data = p + 8;
                    // 属性数据区越界：视为损坏，停止本层扫描
                    if !self.prop_data_ok(p, len) {
                        return None;
                    }
                    let pname = self.string_at(nameoff);
                    if pname == name {
                        let value = unsafe { slice::from_raw_parts(data as *const u8, len) };
                        return Some(Prop { value });
                    }
                    p = data + align4(len);
                }
                FDT_NOP => {}
                FDT_END => return None,
                _ => return None,
            }
        }
    }

    /// 按 compatible 字符串遍历整棵树，返回第一个匹配节点。
    fn search_compatible(&self, mut p: usize, compat: &str) -> Option<usize> {
        loop {
            let t = self.token(p);
            p += 4;
            match t {
                FDT_BEGIN_NODE => {
                    let (_, child) = self.read_cstr(p);
                    // 检查该节点自身
                    if let Some(prop) = self.prop_at(child, "compatible") {
                        if prop.value.split(|&b| b == 0).any(|s| s == compat.as_bytes()) {
                            return Some(child);
                        }
                    }
                    // 递归进入子节点
                    if let Some(found) = self.search_compatible(child, compat) {
                        return Some(found);
                    }
                    p = self.skip_subtree(child);
                }
                FDT_END_NODE => return None,
                FDT_PROP => {
                    let len = self.token(p) as usize;
                    if !self.prop_data_ok(p, len) {
                        return None;
                    }
                    p += 8 + align4(len);
                }
                FDT_NOP => {}
                FDT_END => return None,
                _ => return None,
            }
        }
    }

    /// 按绝对路径查找节点（如 `/chosen`、`/memory`、`/soc/pl011`）。
    pub fn node(&self, path: &str) -> Option<Node> {
        // 结构块以根节点 BEGIN_NODE（名字为空串）开头
        let mut p = self.struct_base;
        if self.token(p) != FDT_BEGIN_NODE {
            return None;
        }
        p += 4;
        let (_, p) = self.read_cstr(p); // 跳过根节点名字（空串）
        let path = path.trim_start_matches('/');
        if path.is_empty() {
            return Some(Node { offset: p });
        }
        self.search(p, path).map(|off| Node { offset: off })
    }

    /// 按 compatible 字符串查找第一个匹配节点（如 `"arm,pl011"`）。
    pub fn find_compatible(&self, compat: &str) -> Option<Node> {
        let mut p = self.struct_base;
        if self.token(p) != FDT_BEGIN_NODE {
            return None;
        }
        p += 4;
        let (_, p) = self.read_cstr(p);
        self.search_compatible(p, compat).map(|off| Node { offset: off })
    }

    /// 按 compatible 字符串遍历整棵树，把所有匹配节点写入 `out`，返回个数。
    pub fn find_all_compatible(&self, compat: &str, out: &mut [Node]) -> usize {
        let mut p = self.struct_base;
        if self.token(p) != FDT_BEGIN_NODE {
            return 0;
        }
        p += 4;
        let (_, p) = self.read_cstr(p);
        let mut n = 0usize;
        self.search_compatible_all(p, compat, out, &mut n);
        n
    }

    /// `find_all_compatible` 的递归实现，累计写入 `out` 并由 `count` 记录个数。
    fn search_compatible_all(
        &self,
        mut p: usize,
        compat: &str,
        out: &mut [Node],
        count: &mut usize,
    ) {
        loop {
            let t = self.token(p);
            p += 4;
            match t {
                FDT_BEGIN_NODE => {
                    let (_, child) = self.read_cstr(p);
                    if *count < out.len() {
                        if let Some(prop) = self.prop_at(child, "compatible") {
                            if prop.value.split(|&b| b == 0).any(|s| s == compat.as_bytes()) {
                                out[*count] = Node { offset: child };
                                *count += 1;
                            }
                        }
                    }
                    self.search_compatible_all(child, compat, out, count);
                    p = self.skip_subtree(child);
                }
                FDT_END_NODE => return,
                FDT_PROP => {
                    let len = self.token(p) as usize;
                    if !self.prop_data_ok(p, len) {
                        return;
                    }
                    p += 8 + align4(len);
                }
                FDT_NOP => {}
                FDT_END => return,
                _ => return,
            }
        }
    }

    /// 按属性名 + 字符串值查找第一个匹配节点（如 `device_type = "memory"`）。
    pub fn find_property(&self, name: &str, value: &str) -> Option<Node> {
        let mut p = self.struct_base;
        if self.token(p) != FDT_BEGIN_NODE {
            return None;
        }
        p += 4;
        let (_, p) = self.read_cstr(p);
        self.search_property(p, name, value).map(|off| Node { offset: off })
    }

    /// 遍历 `p` 起始的子树，查找第一个 `name` 属性值等于 `value` 的节点。
    fn search_property(&self, mut p: usize, name: &str, value: &str) -> Option<usize> {
        loop {
            let t = self.token(p);
            p += 4;
            match t {
                FDT_BEGIN_NODE => {
                    let (_, child) = self.read_cstr(p);
                    if let Some(prop) = self.prop_at(child, name) {
                        let end = prop.value.iter().position(|&b| b == 0).unwrap_or(prop.value.len());
                        if &prop.value[..end] == value.as_bytes() {
                            return Some(child);
                        }
                    }
                    if let Some(found) = self.search_property(child, name, value) {
                        return Some(found);
                    }
                    p = self.skip_subtree(child);
                }
                FDT_END_NODE => return None,
                FDT_PROP => {
                    let len = self.token(p) as usize;
                    if !self.prop_data_ok(p, len) {
                        return None;
                    }
                    p += 8 + align4(len);
                }
                FDT_NOP => {}
                FDT_END => return None,
                _ => return None,
            }
        }
    }

    /// 字符串块大小（调试用）。
    #[allow(dead_code)]
    pub fn strings_size(&self) -> usize {
        self.strings_size
    }
}

impl Node {
    /// 空节点（offset=0），供 `find_all_compatible` 的结果数组初始化。
    pub const fn empty() -> Self {
        Node { offset: 0 }
    }

    /// 读取属性（原始字节）。
    pub fn prop(self, fdt: &Fdt, name: &str) -> Option<Prop<'static>> {
        fdt.prop_at(self.offset, name)
    }

    /// 读取字符串属性（去掉末尾 NUL）。
    pub fn prop_str(self, fdt: &Fdt, name: &str) -> Option<&'static str> {
        let p = self.prop(fdt, name)?;
        let end = p.value.iter().position(|&b| b == 0).unwrap_or(p.value.len());
        // 安全 UTF-8（DTB 属性值可能含任意字节；from_utf8_unchecked 为 UB）
        Some(core::str::from_utf8(&p.value[..end]).unwrap_or(""))
    }

    /// 读取第 index 个 u32 属性值（大端）。
    pub fn prop_u32(self, fdt: &Fdt, name: &str, index: usize) -> Option<u32> {
        let p = self.prop(fdt, name)?;
        let off = index * 4;
        if off + 4 > p.value.len() {
            return None;
        }
        Some(u32::from_be_bytes([
            p.value[off],
            p.value[off + 1],
            p.value[off + 2],
            p.value[off + 3],
        ]))
    }

    /// 读取两个 32 位 cell 组成的 64 位值（第 index 个 64 位单元）。
    pub fn prop_u64(self, fdt: &Fdt, name: &str, index: usize) -> Option<u64> {
        let hi = self.prop_u32(fdt, name, index * 2)? as u64;
        let lo = self.prop_u32(fdt, name, index * 2 + 1)? as u64;
        Some((hi << 32) | lo)
    }

    /// 读取 `reg` 属性第 index 个 `(addr, size)` 对（假定 #address-cells = #size-cells = 2）。
    pub fn reg(self, fdt: &Fdt, index: usize) -> Option<(u64, u64)> {
        let addr = self.prop_u64(fdt, "reg", index * 2)?;
        let size = self.prop_u64(fdt, "reg", index * 2 + 1)?;
        Some((addr, size))
    }

    /// 读取 `interrupts` 属性第 index 个三元组 `(type, number, flags)`（GIC #interrupt-cells = 3）。
    ///
    /// RISC-V 平台额外处理两种形态：`interrupts` 的 PLIC 1 单元直通，以及
    /// QEMU riscv virt 外设实际使用的 `interrupts-extended`（每条 =
    /// `<phandle><arg...>`，目标 PLIC 的 #interrupt-cells = 1，故第 index
    /// 条的参数在 cell 偏移 index*2 + 1；phandle 本身不解析，平台目标只有
    /// QEMU virt）。
    pub fn interrupt(self, fdt: &Fdt, index: usize) -> Option<(u32, u32, u32)> {
        // GIC 3 单元（type, number, flags）—— aarch64 DTB
        if let (Some(t), Some(n), Some(f)) = (
            self.prop_u32(fdt, "interrupts", index * 3),
            self.prop_u32(fdt, "interrupts", index * 3 + 1),
            self.prop_u32(fdt, "interrupts", index * 3 + 2),
        ) {
            return Some((t, n, f));
        }
        // PLIC 1 单元（直接 IRQ 号）—— RISC-V DTB（#interrupt-cells = <1>）
        if let Some(n) = self.prop_u32(fdt, "interrupts", index) {
            return Some((PLIC_DIRECT, n, 0));
        }
        // interrupts-extended：跳过 phandle cell，取第 index 条的 PLIC 参数
        if let Some(n) = self.prop_u32(fdt, "interrupts-extended", index * 2 + 1) {
            return Some((PLIC_DIRECT, n, 0));
        }
        None
    }
}

/// 中断单元类型标记：PLIC 单 cell 直通（gic_irq 对其返回原号）。
pub const PLIC_DIRECT: u32 = 2;

/// 将 FDT 中断单元 `(type, number)` 转换为 GIC 中断号：
/// SPI（type=0）→ 32 + number，PPI（type=1）→ 16 + number。
pub fn gic_irq(id_type: u32, number: u32) -> u32 {
    match id_type {
        0 => 32 + number, // SPI
        1 => 16 + number, // PPI
        _ => number,
    }
}
