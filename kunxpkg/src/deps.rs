//! 依赖解析引擎（no_std、无分配）：
//! - 系统库（libc/libm/kunx-api）由系统提供，版本固定；
//! - 应用依赖按语义版本约束匹配：已安装版本满足 → OK；
//!   否则在可用索引中存在 → 递归解析其依赖（`Resolver`，安装闭包）；
//! - 冲突检测：`Conflicts:`（与已安装包/提供者冲突）与 `Provides:` 虚拟名占用。
//!
//! 闭包解析（`Resolver`）：对目标包做后序 DFS——依赖全部入计划后目标才入
//! 计划，保证安装顺序；DFS 栈上重复出现同名包即依赖环（明确报错，不无限
//! 递归）；索引版本不满足约束、索引缺失均报 Missing。闭包大小 / 栈深有
//! 静态上限，防御恶意超长依赖链。
//!
//! 说明：这是「预检」级解析（可行性 + 安装顺序），不做全局 SAT 求解；
//! 多包同时升级的完整规划留作后续（见 docs/kunx-package-manager.md）。

use crate::format::NameRef;
use crate::semver;
use crate::trust;

/// 系统提供的库（name, version 字符串）。
pub const SYSTEM_LIBS: &[(&[u8], &[u8])] = &[
    (b"libc", trust::SYSTEM_LIBC),
    (b"libm", trust::SYSTEM_LIBM),
    (b"kunx-api", b"1.0.0"),
];

/// 已安装版本检查：`(name, constraint)` → 已安装版本是否满足约束。
pub type InstalledCheck = fn(&[u8], &[u8]) -> bool;

/// 可用索引存在性检查：`name` → 索引中是否存在（可安装）。
pub type AvailableCheck = fn(&[u8]) -> bool;

/// 已安装包是否提供某虚拟名 / 冲突名。
pub type ProvidesCheck = fn(&[u8]) -> bool;

/// 单条依赖是否可满足：系统库 → 已安装 → 可用索引。
/// 依赖环由深度上限（8 层）兜底。
pub fn dep_satisfiable(
    name: &[u8],
    constraint: &[u8],
    depth: usize,
    installed: InstalledCheck,
    available: AvailableCheck,
) -> bool {
    if depth > 8 {
        return false;
    }
    let (cs, cn) = match semver::parse_constraints(constraint) {
        Some(c) => c,
        None => return false,
    };
    for (sn, sv) in SYSTEM_LIBS {
        if *sn == name {
            if let Some(v) = semver::parse(sv) {
                return semver::match_all(&v, &cs, cn);
            }
            return false;
        }
    }
    if installed(name, constraint) {
        return true;
    }
    available(name)
}

/// 校验包依赖与冲突。`installed`/`available`/`installed_provides` 由调用方提供。
pub fn check<'a>(
    deps: &[(NameRef<'a>, &'a [u8])],
    dep_count: usize,
    provides: &[&'a [u8]],
    provide_count: usize,
    conflicts: &[&'a [u8]],
    conflict_count: usize,
    installed: InstalledCheck,
    available: AvailableCheck,
    installed_provides: ProvidesCheck,
) -> Result<(), DepError<'a>> {
    for i in 0..dep_count {
        let (name, c) = (deps[i].0 .0, deps[i].1);
        if !dep_satisfiable(name, c, 0, installed, available) {
            return Err(DepError::Missing(name));
        }
    }
    for i in 0..conflict_count {
        let cname = conflicts[i];
        if installed(cname, b"") || installed_provides(cname) {
            return Err(DepError::Conflict(cname));
        }
    }
    for i in 0..provide_count {
        let pname = provides[i];
        if installed_provides(pname) {
            return Err(DepError::ProvidesTaken(pname));
        }
    }
    Ok(())
}

/// 依赖检查失败原因。
#[derive(Clone, Copy, Debug)]
pub enum DepError<'a> {
    Missing(&'a [u8]),
    Conflict(&'a [u8]),
    ProvidesTaken(&'a [u8]),
}

// ==================== 递归闭包解析 ====================

/// 闭包内包名最长（与索引记录名一致）。
pub const NAME_MAX: usize = 48;
/// 安装计划容量（依赖闭包内软件包数上限）。
pub const CLOSURE_MAX: usize = 24;
/// DFS 深度上限（依赖链层数；环由栈检测拦截，此为防御兜底）。
pub const DEPTH_MAX: usize = 12;
/// 索引 Depends 原始串缓冲。
const DEPS_RAW_MAX: usize = 256;

/// 索引依赖列表查询：`name` → 把 "n1 c1,n2 c2" 拷入 out 返回长度；
/// 包不在索引中返回 usize::MAX。
pub type DepsLookup = fn(&[u8], &mut [u8]) -> usize;

/// 索引版本查询：`name` → 版本串拷入 out 返回长度；不在索引返回 0。
pub type VersionLookup = fn(&[u8], &mut [u8]) -> usize;

/// 安装计划：后序 DFS 产物，依赖在前、目标在后。
pub struct Plan {
    names: [[u8; NAME_MAX]; CLOSURE_MAX],
    lens: [usize; CLOSURE_MAX],
    pub count: usize,
}

impl Plan {
    pub const fn new() -> Plan {
        Plan { names: [[0; NAME_MAX]; CLOSURE_MAX], lens: [0; CLOSURE_MAX], count: 0 }
    }

    pub fn name(&self, i: usize) -> &[u8] {
        &self.names[i][..self.lens[i]]
    }

    pub fn contains(&self, n: &[u8]) -> bool {
        for i in 0..self.count {
            if self.name(i) == n {
                return true;
            }
        }
        false
    }

    /// 追加一个包名（调用方显式补目标用）；超限返回 false。
    pub fn push_name(&mut self, n: &[u8]) -> bool {
        if self.count >= CLOSURE_MAX || n.is_empty() || n.len() > NAME_MAX {
            return false;
        }
        self.names[self.count][..n.len()].copy_from_slice(n);
        self.lens[self.count] = n.len();
        self.count += 1;
        true
    }
}

/// 解析失败原因（细节见 Resolver::fail_name / DFS 栈）。
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ResolveFail {
    /// 未安装、索引缺失或索引版本不满足约束。
    Missing,
    /// 依赖环（fail_name 为闭环点，栈即环路径）。
    Cycle,
    /// 闭包超过 CLOSURE_MAX。
    ClosureFull,
    /// 依赖链超过 DEPTH_MAX 层。
    DepthExceeded,
}

/// 递归依赖闭包解析器。
pub struct Resolver {
    stack: [[u8; NAME_MAX]; DEPTH_MAX],
    stack_lens: [usize; DEPTH_MAX],
    depth: usize,
    /// 失败类型。
    pub fail: ResolveFail,
    /// 失败主体（Missing 的依赖名 / Cycle 的闭环点名）。
    pub fail_name: [u8; NAME_MAX],
    pub fail_name_len: usize,
}

impl Resolver {
    pub const fn new() -> Resolver {
        Resolver {
            stack: [[0; NAME_MAX]; DEPTH_MAX],
            stack_lens: [0; DEPTH_MAX],
            depth: 0,
            fail: ResolveFail::Missing,
            fail_name: [0; NAME_MAX],
            fail_name_len: 0,
        }
    }

    /// DFS 栈上第 i 个包名（环路径打印用）。
    pub fn stack_name(&self, i: usize) -> &[u8] {
        &self.stack[i][..self.stack_lens[i]]
    }

    pub fn depth(&self) -> usize {
        self.depth
    }

    /// 解析 `root` 的依赖闭包：需要安装的包全部进入 `plan`（root 最后）。
    /// root 已安装且满足时返回空计划（是否重装由调用方决定）。
    pub fn resolve(
        &mut self,
        root: &[u8],
        plan: &mut Plan,
        installed: InstalledCheck,
        available: AvailableCheck,
        deps_of: DepsLookup,
        version_of: VersionLookup,
    ) -> Result<(), ResolveFail> {
        self.visit(root, b"", plan, installed, available, deps_of, version_of)
    }

    /// 从一组依赖边解析闭包（install-file：目标来自本地文件，本身不入计划）。
    pub fn resolve_deps(
        &mut self,
        deps: &[(&[u8], &[u8])],
        plan: &mut Plan,
        installed: InstalledCheck,
        available: AvailableCheck,
        deps_of: DepsLookup,
        version_of: VersionLookup,
    ) -> Result<(), ResolveFail> {
        for (n, c) in deps {
            self.visit(n, c, plan, installed, available, deps_of, version_of)?;
        }
        Ok(())
    }

    fn visit(
        &mut self,
        name: &[u8],
        constraint: &[u8],
        plan: &mut Plan,
        installed: InstalledCheck,
        available: AvailableCheck,
        deps_of: DepsLookup,
        version_of: VersionLookup,
    ) -> Result<(), ResolveFail> {
        if name.is_empty() || name.len() > NAME_MAX {
            return self.fail_with(ResolveFail::Missing, name);
        }
        // 系统库：由系统提供，版本固定
        for (sn, sv) in SYSTEM_LIBS {
            if *sn == name {
                if constraint.is_empty() {
                    return Ok(());
                }
                return match (semver::parse(sv), semver::parse_constraints(constraint)) {
                    (Some(v), Some((cs, cn))) if semver::match_all(&v, &cs, cn) => Ok(()),
                    _ => self.fail_with(ResolveFail::Missing, name),
                };
            }
        }
        // 已安装且满足约束
        if installed(name, constraint) {
            return Ok(());
        }
        // 索引必须存在（否则装不出，明确失败而非放行）
        if !available(name) {
            return self.fail_with(ResolveFail::Missing, name);
        }
        // 环检测：name 已在当前 DFS 路径上
        for i in 0..self.depth {
            if self.stack_name(i) == name {
                return self.fail_with(ResolveFail::Cycle, name);
            }
        }
        if self.depth >= DEPTH_MAX {
            return self.fail_with(ResolveFail::DepthExceeded, name);
        }
        // 索引版本必须满足父边约束（先于菱形去重：第二条边可能比第一条更
        // 严，如 A→B(>=1) 且 A→C→B(>=2)——去重短路跳过校验会装出 C 的坏依赖）
        if !constraint.is_empty() {
            let mut vb = [0u8; 32];
            let vn = version_of(name, &mut vb);
            let ok = vn > 0
                && match (semver::parse(&vb[..vn]), semver::parse_constraints(constraint)) {
                    (Some(v), Some((cs, cn))) => semver::match_all(&v, &cs, cn),
                    _ => false,
                };
            if !ok {
                return self.fail_with(ResolveFail::Missing, name);
            }
        }
        // 菱形依赖：另一条路径已解析过（约束已在上面校验通过）
        if plan.contains(name) {
            return Ok(());
        }
        // 递归解析本包依赖（索引 Depends 字段）
        let mut raw = [0u8; DEPS_RAW_MAX];
        let rn = deps_of(name, &mut raw);
        if rn == usize::MAX || rn > DEPS_RAW_MAX {
            return self.fail_with(ResolveFail::Missing, name);
        }
        self.stack[self.depth][..name.len()].copy_from_slice(name);
        self.stack_lens[self.depth] = name.len();
        self.depth += 1;
        for part in raw[..rn].split(|&c| c == b',') {
            let p = trim(part);
            if p.is_empty() {
                continue;
            }
            let (dn, dc) = match p.iter().position(|&c| c == b' ' || c == b'\t') {
                Some(sp) => (&p[..sp], trim(&p[sp + 1..])),
                None => (p, &[][..]),
            };
            self.visit(dn, dc, plan, installed, available, deps_of, version_of)?;
        }
        self.depth -= 1;
        // 后序入计划：依赖全部就绪后才是本包
        if !plan.push_name(name) {
            return self.fail_with(ResolveFail::ClosureFull, name);
        }
        Ok(())
    }

    fn fail_with(&mut self, kind: ResolveFail, name: &[u8]) -> Result<(), ResolveFail> {
        self.fail = kind;
        let c = name.len().min(NAME_MAX);
        self.fail_name[..c].copy_from_slice(&name[..c]);
        self.fail_name_len = c;
        Err(kind)
    }
}

fn trim(b: &[u8]) -> &[u8] {
    let mut s = 0usize;
    let mut e = b.len();
    while s < e && (b[s] == b' ' || b[s] == b'\t') {
        s += 1;
    }
    while e > s && (b[e - 1] == b' ' || b[e - 1] == b'\t' || b[e - 1] == b'\r') {
        e -= 1;
    }
    &b[s..e]
}
