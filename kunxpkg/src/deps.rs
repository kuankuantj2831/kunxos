//! 依赖解析引擎（no_std、无分配）：
//! - 系统库（libc/libm/kunx-api）由系统提供，版本固定；
//! - 应用依赖按语义版本约束匹配：已安装版本满足 → OK；
//!   否则在可用索引中存在 → OK（其自身依赖由调用方预检）；
//! - 冲突检测：`Conflicts:`（与已安装包/提供者冲突）与 `Provides:` 虚拟名占用。
//!
//! 说明：这是「预检」级解析（单包安装可行性），不做全局 SAT 求解；
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
