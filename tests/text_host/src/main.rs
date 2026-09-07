#[path = "../../../shared/l10n.rs"]
mod l10n;
use l10n::*;
fn main() {
    // 语言切换
    set_lang(L10nLang::Zh);
    assert_eq!(tr("app.settings"), "设置");
    set_lang(L10nLang::En);
    assert_eq!(tr("app.settings"), "Settings");
    set_lang(L10nLang::Ar);
    assert_eq!(tr("app.settings"), "الإعدادات");
    // 复数规则：zh(1 form) / en(2) / ar(6)
    let zh = plural_rule(L10nLang::Zh);
    let en = plural_rule(L10nLang::En);
    let ar = plural_rule(L10nLang::Ar);
    for n in [0i64, 1, 2, 3, 11, 22, 101, 102] {
        println!("n={} zh={} en={} ar={}", n,
            plural_index(&zh, n), plural_index(&en, n), plural_index(&ar, n));
    }
    assert_eq!(plural_index(&zh, 5), 0);
    assert_eq!(plural_index(&en, 1), 0);
    assert_eq!(plural_index(&en, 0), 1);
    assert_eq!(plural_index(&ar, 0), 0);
    assert_eq!(plural_index(&ar, 1), 1);
    assert_eq!(plural_index(&ar, 2), 2);
    assert_eq!(plural_index(&ar, 3), 3);
    assert_eq!(plural_index(&ar, 11), 4);
    assert_eq!(plural_index(&ar, 100), 5);
    assert_eq!(plural_index(&ar, 102), 5);
    // 语言代码
    assert_eq!(lang().code(), "ar-SA");
    println!("ALL L10N OK");
}
