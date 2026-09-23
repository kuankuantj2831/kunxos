//! KunxOS 本地化框架（gettext 风格子集）。
//!
//! - 字符串目录：`(msgid, zh, en, ar)` 静态表；`tr(id)` 按当前语言取串；
//! - 运行时语言切换：`set_lang`（从 `/data/oobe/lang` 读取或应用内切换）；
//! - 复数规则：gettext `plural=...` 表达式求值器（支持 zh/en/ar 等形态）；
//! - 语言：简体中文 / English / العربية。
//!
//! 宿主可测（无系统调用；`lang_init` 的 /data 读取以 cfg 门控）。

extern crate alloc;

// ==================== 语言 ====================

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum L10nLang {
    Zh,
    En,
    Ar,
}

impl L10nLang {
    /// BCP-47 / locale 代码。
    pub fn code(&self) -> &'static str {
        match self {
            L10nLang::Zh => "zh-CN",
            L10nLang::En => "en-US",
            L10nLang::Ar => "ar-SA",
        }
    }

    /// 两字母语言代码。
    pub fn short(&self) -> &'static str {
        match self {
            L10nLang::Zh => "zh",
            L10nLang::En => "en",
            L10nLang::Ar => "ar",
        }
    }
}

static mut CURRENT_LANG: L10nLang = L10nLang::Zh;

/// 当前语言。
pub fn lang() -> L10nLang {
    unsafe { CURRENT_LANG }
}

/// 切换语言（运行时；调用方负责重绘 UI）。
pub fn set_lang(l: L10nLang) {
    unsafe { CURRENT_LANG = l; }
}

/// 从 /data/oobe/lang 读取语言设置（含 "ar"→阿拉伯文，"en"→英文，默认中文）。
#[cfg(target_os = "none")]
pub fn lang_init() {
    // 简化：不引入额外 syscall（ttf_text/app_kit 已有读文件封装）。
    // 这里由调用方（app_kit::kit_init）读取后调用 set_lang。
}

// ==================== 字符串目录 ====================

/// 消息条目（msgid + 三种语言）。
pub struct Msg {
    pub id: &'static str,
    pub zh: &'static str,
    pub en: &'static str,
    pub ar: &'static str,
}

/// 取当前语言下的字符串；未命中回退 zh。
/// 生命周期跟随入参：兜底分支透传 id，字面量调用点推断为 'static。
pub fn tr<'a>(id: &'a str) -> &'a str {
    let l = lang();
    for m in CATALOG {
        if m.id == id {
            return match l {
                L10nLang::Zh => m.zh,
                L10nLang::En => m.en,
                L10nLang::Ar => m.ar,
            };
        }
    }
    id
}

/// 取指定语言下的字符串（不切换当前语言）。
pub fn tr_in<'a>(id: &'a str, l: L10nLang) -> &'a str {
    for m in CATALOG {
        if m.id == id {
            return match l {
                L10nLang::Zh => m.zh,
                L10nLang::En => m.en,
                L10nLang::Ar => m.ar,
            };
        }
    }
    id
}

/// 按语言取"语言显示名"。
pub fn lang_display_name(l: L10nLang) -> &'static str {
    match l {
        L10nLang::Zh => "简体中文",
        L10nLang::En => "English",
        L10nLang::Ar => "العربية",
    }
}

// 系统常用字符串目录（应用可扩展）。
pub static CATALOG: &[Msg] = &[
    Msg { id: "app.settings", zh: "设置", en: "Settings", ar: "الإعدادات" },
    Msg { id: "app.phone", zh: "电话", en: "Phone", ar: "الهاتف" },
    Msg { id: "app.sms", zh: "短信", en: "Messages", ar: "الرسائل" },
    Msg { id: "app.browser", zh: "浏览器", en: "Browser", ar: "المتصفح" },
    Msg { id: "app.camera", zh: "相机", en: "Camera", ar: "الكاميرا" },
    Msg { id: "app.calculator", zh: "计算器", en: "Calculator", ar: "الحاسبة" },
    Msg { id: "app.terminal", zh: "终端", en: "Terminal", ar: "الطرفية" },
    Msg { id: "app.clock", zh: "时钟", en: "Clock", ar: "الساعة" },
    Msg { id: "app.calendar", zh: "日历", en: "Calendar", ar: "التقويم" },
    Msg { id: "app.files", zh: "文件", en: "Files", ar: "الملفات" },
    Msg { id: "app.contacts", zh: "联系人", en: "Contacts", ar: "جهات الاتصال" },
    Msg { id: "app.gallery", zh: "相册", en: "Gallery", ar: "المعرض" },
    Msg { id: "app.music", zh: "音乐", en: "Music", ar: "الموسيقى" },
    Msg { id: "app.mail", zh: "邮件", en: "Mail", ar: "البريد" },
    Msg { id: "app.map", zh: "地图", en: "Map", ar: "الخريطة" },
    Msg { id: "app.weather", zh: "天气", en: "Weather", ar: "الطقس" },
    Msg { id: "app.store", zh: "应用商店", en: "App Store", ar: "متجر التطبيقات" },
    Msg { id: "app.notes", zh: "备忘录", en: "Notes", ar: "الملاحظات" },
    Msg { id: "common.ok", zh: "确定", en: "OK", ar: "موافق" },
    Msg { id: "common.cancel", zh: "取消", en: "Cancel", ar: "إلغاء" },
    Msg { id: "common.back", zh: "返回", en: "Back", ar: "رجوع" },
    Msg { id: "common.delete", zh: "删除", en: "Delete", ar: "حذف" },
    Msg { id: "common.save", zh: "保存", en: "Save", ar: "حفظ" },
    Msg { id: "common.search", zh: "搜索", en: "Search", ar: "بحث" },
    Msg { id: "common.empty", zh: "暂无内容", en: "Nothing here", ar: "لا يوجد محتوى" },
    Msg { id: "common.loading", zh: "加载中…", en: "Loading…", ar: "جارٍ التحميل…" },
    Msg { id: "nav.home", zh: "桌面", en: "Home", ar: "الرئيسية" },
    Msg { id: "nav.recent", zh: "最近任务", en: "Recent", ar: "الأخيرة" },
    Msg { id: "ime.zh", zh: "中文", en: "中文", ar: "الصينية" },
    Msg { id: "ime.en", zh: "ABC", en: "ABC", ar: "ABC" },
    Msg { id: "ime.ar", zh: "عربي", en: "عربي", ar: "عربي" },
    Msg { id: "ime.space", zh: "空格", en: "Space", ar: "مسافة" },
    Msg { id: "ime.shift", zh: "大写", en: "Shift", ar: "تحويل" },
    Msg { id: "ime.del", zh: "删除", en: "Delete", ar: "حذف" },
    Msg { id: "ime.enter", zh: "回车", en: "Enter", ar: "إدخال" },
    Msg { id: "ime.123", zh: "符号", en: "123", ar: "123" },
    Msg { id: "ime.zh_hint", zh: "拼音", en: "Pinyin", ar: "بينيين" },
    Msg { id: "settings.language", zh: "语言", en: "Language", ar: "اللغة" },
    Msg { id: "settings.timezone", zh: "时区", en: "Timezone", ar: "المنطقة الزمنية" },
    Msg { id: "settings.display", zh: "显示与亮度", en: "Display & Brightness", ar: "الشاشة والسطوع" },
    Msg { id: "settings.theme", zh: "主题", en: "Theme", ar: "المظهر" },
    Msg { id: "settings.network", zh: "网络", en: "Network", ar: "الشبكة" },
    Msg { id: "settings.security", zh: "安全", en: "Security", ar: "الأمان" },
    Msg { id: "settings.about", zh: "关于", en: "About", ar: "حول" },
];

// ==================== 复数规则（gettext plural 表达式求值） ====================

/// gettext 复数规则。
pub struct PluralRule {
    pub nplurals: u8,
    /// 表达式（如 `(n != 1)`、`n==0 ? 0 : n==1 ? 1 : 2`）。
    pub formula: &'static str,
}

/// 常见语言复数形态（nplurals 与 formula 与 gettext 一致）。
pub fn plural_rule(l: L10nLang) -> PluralRule {
    match l {
        L10nLang::Zh => PluralRule { nplurals: 1, formula: "0" },
        L10nLang::En => PluralRule { nplurals: 2, formula: "(n != 1)" },
        L10nLang::Ar => PluralRule {
            nplurals: 6,
            formula: "n==0 ? 0 : n==1 ? 1 : n==2 ? 2 : n%100>=3 && n%100<=10 ? 3 : n%100>=11 ? 4 : 5",
        },
    }
}

/// 计算复数形态索引（0..nplurals）。
pub fn plural_index(rule: &PluralRule, n: i64) -> u32 {
    let v = eval_plural(rule.formula, n).unwrap_or(0);
    if v < 0 {
        0
    } else {
        (v as u64).min(rule.nplurals as u64 - 1) as u32
    }
}

// ---- 迷你表达式求值器（仅 gettext plural 语法子集） ----

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
        i += 1;
    }
    i
}

fn parse_primary(b: &[u8], i: usize, n: i64) -> Option<(i64, usize)> {
    let i = skip_ws(b, i);
    if i >= b.len() {
        return None;
    }
    match b[i] {
        b'n' => Some((n, i + 1)),
        b'(' => {
            let (v, j) = parse_expr(b, i + 1, n)?;
            let j = skip_ws(b, j);
            if j < b.len() && b[j] == b')' {
                Some((v, j + 1))
            } else {
                None
            }
        }
        b'0'..=b'9' => {
            let mut j = i;
            let mut v: i64 = 0;
            while j < b.len() && b[j].is_ascii_digit() {
                v = v * 10 + (b[j] - b'0') as i64;
                j += 1;
            }
            Some((v, j))
        }
        b'-' => {
            let (v, j) = parse_primary(b, i + 1, n)?;
            Some((-v, j))
        }
        _ => None,
    }
}

fn parse_mul(b: &[u8], i: usize, n: i64) -> Option<(i64, usize)> {
    let (mut lhs, mut i) = parse_primary(b, i, n)?;
    loop {
        let i2 = skip_ws(b, i);
        if i2 >= b.len() {
            break;
        }
        match b[i2] {
            b'*' => {
                let (rhs, j) = parse_primary(b, i2 + 1, n)?;
                lhs *= rhs;
                i = j;
            }
            b'/' => {
                let (rhs, j) = parse_primary(b, i2 + 1, n)?;
                if rhs == 0 {
                    return None;
                }
                lhs /= rhs;
                i = j;
            }
            b'%' => {
                let (rhs, j) = parse_primary(b, i2 + 1, n)?;
                if rhs == 0 {
                    return None;
                }
                lhs %= rhs;
                i = j;
            }
            _ => {
                i = i2;
                break;
            }
        }
    }
    Some((lhs, i))
}

fn parse_add(b: &[u8], i: usize, n: i64) -> Option<(i64, usize)> {
    let (mut lhs, mut i) = parse_mul(b, i, n)?;
    loop {
        let i2 = skip_ws(b, i);
        if i2 >= b.len() {
            break;
        }
        match b[i2] {
            b'+' => {
                let (rhs, j) = parse_mul(b, i2 + 1, n)?;
                lhs += rhs;
                i = j;
            }
            b'-' => {
                let (rhs, j) = parse_mul(b, i2 + 1, n)?;
                lhs -= rhs;
                i = j;
            }
            _ => {
                i = i2;
                break;
            }
        }
    }
    Some((lhs, i))
}

fn parse_cmp(b: &[u8], i: usize, n: i64) -> Option<(i64, usize)> {
    let (lhs, i) = parse_add(b, i, n)?;
    let i = skip_ws(b, i);
    if i + 1 < b.len() {
        let op = &b[i..i + 2];
        let j = match op {
            b"==" => Some(i + 2),
            b"!=" => Some(i + 2),
            b"<=" => Some(i + 2),
            b">=" => Some(i + 2),
            _ => None,
        };
        if let Some(j) = j {
            let (rhs, k) = parse_add(b, j, n)?;
            let v = match op {
                b"==" => (lhs == rhs) as i64,
                b"!=" => (lhs != rhs) as i64,
                b"<=" => (lhs <= rhs) as i64,
                b">=" => (lhs >= rhs) as i64,
                _ => 0,
            };
            return Some((v, k));
        }
    }
    if i < b.len() && (b[i] == b'<' || b[i] == b'>') {
        let (rhs, k) = parse_add(b, i + 1, n)?;
        let v = if b[i] == b'<' { (lhs < rhs) as i64 } else { (lhs > rhs) as i64 };
        return Some((v, k));
    }
    Some((lhs, i))
}

/// && 链（左结合，高于 ?: 低于 ||）：此前 && 在 parse_cmp 内只消费一次，
/// 阿拉伯语 `n%100>=3 && n%100<=10` 解析后剩余字节使 eval 失败 → 恒回退
/// 形态 0（六种复数形态全部退化）。
fn parse_and(b: &[u8], i: usize, n: i64) -> Option<(i64, usize)> {
    let (mut v, mut i) = parse_cmp(b, i, n)?;
    loop {
        let j = skip_ws(b, i);
        if j + 1 < b.len() && &b[j..j + 2] == b"&&" {
            let (rhs, k) = parse_cmp(b, j + 2, n)?;
            v = ((v != 0) && (rhs != 0)) as i64;
            i = k;
        } else {
            break;
        }
    }
    Some((v, i))
}

/// || 链（左结合，低于 &&）。
fn parse_or(b: &[u8], i: usize, n: i64) -> Option<(i64, usize)> {
    let (mut v, mut i) = parse_and(b, i, n)?;
    loop {
        let j = skip_ws(b, i);
        if j + 1 < b.len() && &b[j..j + 2] == b"||" {
            let (rhs, k) = parse_and(b, j + 2, n)?;
            v = ((v != 0) || (rhs != 0)) as i64;
            i = k;
        } else {
            break;
        }
    }
    Some((v, i))
}

fn parse_expr(b: &[u8], i: usize, n: i64) -> Option<(i64, usize)> {
    // 三元 ?: 右结合；优先级 ?: < || < && < 比较
    let (cond, i) = parse_or(b, i, n)?;
    let i = skip_ws(b, i);
    if i < b.len() && b[i] == b'?' {
        let (a, j) = parse_expr(b, i + 1, n)?;
        let j = skip_ws(b, j);
        if j < b.len() && b[j] == b':' {
            let (c, k) = parse_expr(b, j + 1, n)?;
            return Some((if cond != 0 { a } else { c }, k));
        }
        return None;
    }
    Some((cond, i))
}

/// 求值 plural 表达式，n 为数量。失败返回 None。
pub fn eval_plural(formula: &str, n: i64) -> Option<i64> {
    let b = formula.as_bytes();
    let (v, i) = parse_expr(b, 0, n)?;
    let i = skip_ws(b, i);
    if i == b.len() {
        Some(v)
    } else {
        None
    }
}
