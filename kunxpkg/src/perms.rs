//! 权限注册表：manifest 中声明的权限名称、风险级别与中文标签。
//!
//! 权限在**安装时**声明（manifest.permissions），由用户授权（CLI 交互 / 商店
//! 权限对话框），授权结果记录到 `/var/lib/pkg/permissions/<name>` 与已安装
//! 记录的 `perms=` 字段。内核侧执行挂钩点见 docs/kunx-package-manager.md
//! （当前 VFS 尚无权限模型，执行在服务/IPC 层，属后续工作）。

/// 权限风险级别。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Risk {
    /// 普通权限：默认授予，无敏感数据访问。
    Normal,
    /// 危险权限：涉及隐私/敏感资源，需用户明确授权。
    Dangerous,
}

/// 单个权限定义。
pub struct PermDef {
    pub name: &'static [u8],
    pub label_zh: &'static [u8],
    pub risk: Risk,
}

/// 中文标签（UTF-8 字节）：
/// 访问网络 / 发送通知 / 控制振动 / 读写存储 / 使用相机 / 使用麦克风 /
/// 获取位置 / 读取通讯录 / 读取发送短信 / 拨打电话
macro_rules! zh {
    ($s:expr) => {
        concat!($s).as_bytes()
    };
}

/// 注册表（按需扩充；unknown 权限按 Dangerous 对待）。
pub static PERMS: &[PermDef] = &[
    PermDef { name: b"INTERNET", label_zh: zh!("\u{8bbf}\u{95ee}\u{7f51}\u{7edc}"), risk: Risk::Normal },
    PermDef { name: b"NOTIFICATIONS", label_zh: zh!("\u{53d1}\u{9001}\u{901a}\u{77e5}"), risk: Risk::Normal },
    PermDef { name: b"VIBRATE", label_zh: zh!("\u{63a7}\u{5236}\u{9707}\u{52a8}"), risk: Risk::Normal },
    PermDef { name: b"STORAGE", label_zh: zh!("\u{8bfb}\u{5199}\u{5b58}\u{50a8}"), risk: Risk::Dangerous },
    PermDef { name: b"CAMERA", label_zh: zh!("\u{4f7f}\u{7528}\u{76f8}\u{673a}"), risk: Risk::Dangerous },
    PermDef { name: b"MICROPHONE", label_zh: zh!("\u{4f7f}\u{7528}\u{9ea6}\u{514b}\u{98ce}"), risk: Risk::Dangerous },
    PermDef { name: b"LOCATION", label_zh: zh!("\u{83b7}\u{53d6}\u{4f4d}\u{7f6e}"), risk: Risk::Dangerous },
    PermDef { name: b"CONTACTS", label_zh: zh!("\u{8bfb}\u{53d6}\u{901a}\u{8baf}\u{5f55}"), risk: Risk::Dangerous },
    PermDef { name: b"SMS", label_zh: zh!("\u{8bfb}\u{53d6}\u{53d1}\u{9001}\u{77ed}\u{4fe1}"), risk: Risk::Dangerous },
    PermDef { name: b"PHONE", label_zh: zh!("\u{62e8}\u{6253}\u{7535}\u{8bdd}"), risk: Risk::Dangerous },
];

/// 查询权限定义；未注册返回 None（调用方按 Dangerous 处理）。
pub fn lookup(name: &[u8]) -> Option<&'static PermDef> {
    PERMS.iter().find(|d| d.name == name)
}

/// 判断权限是否危险（未注册的权限一律视为危险）。
pub fn is_dangerous(name: &[u8]) -> bool {
    match lookup(name) {
        Some(d) => d.risk == Risk::Dangerous,
        None => true,
    }
}

/// 权限名是否合法（用于授权记录与校验）：`[A-Z][A-Z0-9_]*`，≤ 32 字节。
pub fn valid_name(name: &[u8]) -> bool {
    if name.is_empty() || name.len() > 32 {
        return false;
    }
    if !name[0].is_ascii_uppercase() {
        return false;
    }
    name.iter().all(|&c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
}

/// 计数某权限列表中的危险权限数。
pub fn count_dangerous(perms: &[&[u8]], n: usize) -> usize {
    (0..n).filter(|&i| is_dangerous(perms[i])).count()
}
