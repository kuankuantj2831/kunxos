//! vCard（RFC 2426 v3.0 / RFC 6350 v4.0）解析与生成。
//!
//! 初始范围：
//!   - 解析：VERSION/FN/N/TEL/EMAIL/ORG/TITLE/NOTE/UID/PHOTO/CATEGORIES/
//!     KIND/MEMBER/REV；行折叠、转义、参数（TYPE/PREF/ENCODING=b/MEDIATYPE）。
//!   - 生成：vCard 3.0 文本（本地新建/修改对象用）。
//!   - 照片：3.0 内联 Base64；4.0 URL / data: URI。解码后存入
//!     Contact.photo（上限 PHOTO_LEN），MIME 类型存 photo_mime。
//!   - 分组：CATEGORIES 与 4.0 KIND=group + MEMBER → Contact.groups。

use crate::sync::model::*;
use crate::sync::util;

use super::ical::{Prop, parse_props, unfold_in_place, unescape_text, escape_text};

// ==================== 属性解析辅助 ====================

/// 参数串中是否包含指定 TYPE 值（逗号分隔，大小写不敏感）。
fn has_type(params: &[u8], want: &[u8]) -> bool {
    let mut rest = params;
    loop {
        let (part, tail) = util::split2(rest, b',');
        let (k, v) = util::split2(part, b'=');
        if util::eq_ignore_case(k, b"TYPE") && util::eq_ignore_case(util::trim(v), want) {
            return true;
        }
        // 3.0 允许裸类型值（无 TYPE= 前缀）：`TEL;CELL:...`
        if k.len() == part.len() && util::eq_ignore_case(util::trim(part), want) {
            return true;
        }
        if tail.is_empty() {
            break;
        }
        rest = tail;
    }
    false
}

fn tel_kind(params: &[u8]) -> u8 {
    if has_type(params, b"CELL") {
        TEL_CELL
    } else if has_type(params, b"FAX") {
        TEL_FAX
    } else if has_type(params, b"HOME") {
        TEL_HOME
    } else if has_type(params, b"WORK") {
        TEL_WORK
    } else if has_type(params, b"VOICE") {
        TEL_VOICE
    } else {
        TEL_VOICE
    }
}

fn email_kind(params: &[u8]) -> u8 {
    if has_type(params, b"HOME") {
        EMAIL_HOME
    } else if has_type(params, b"WORK") {
        EMAIL_WORK
    } else {
        EMAIL_OTHER
    }
}

fn push_tel(c: &mut Contact, value: &[u8], kind: u8) {
    if c.tels_len >= MAX_TELS {
        return;
    }
    let v = util::trim(value);
    c.tels[c.tels_len].kind = kind;
    c.tels[c.tels_len].value_len = util::copy_into(&mut c.tels[c.tels_len].value, v);
    c.tels_len += 1;
}

fn push_email(c: &mut Contact, value: &[u8], kind: u8) {
    if c.emails_len >= MAX_EMAILS {
        return;
    }
    let v = util::trim(value);
    c.emails[c.emails_len].kind = kind;
    c.emails[c.emails_len].value_len = util::copy_into(&mut c.emails[c.emails_len].value, v);
    c.emails_len += 1;
}

fn push_group(c: &mut Contact, value: &[u8]) {
    // CATEGORIES 以逗号分隔
    for part in value.split(|&c| c == b',') {
        if c.groups_len >= MAX_GROUPS {
            break;
        }
        let v = util::trim(part);
        if v.is_empty() {
            continue;
        }
        c.groups[c.groups_len].name_len = util::copy_into(&mut c.groups[c.groups_len].name, v);
        c.groups_len += 1;
    }
}

// ==================== 照片 ====================

fn decode_photo(c: &mut Contact, value: &[u8], params: &[u8], base64_forced: bool) {
    let v = util::trim(value);
    // 4.0 内联：data:image/jpeg;base64,xxxx
    if util::starts_with_ci(v, b"data:") {
        let rest = &v[5..];
        let (mime, payload) = util::split2(rest, b';');
        let payload = if util::starts_with_ci(payload, b"base64,") {
            &payload[7..]
        } else {
            return;
        };
        c.photo_mime_len = util::copy_into(&mut c.photo_mime, mime);
        let n = util::base64_decode(payload, &mut c.photo);
        c.photo_len = n.unwrap_or(0);
        return;
    }
    // 4.0 URL：直接存 URL 字符串（作为照片引用）
    if util::starts_with_ci(v, b"http://") || util::starts_with_ci(v, b"https://") {
        c.photo_mime_len = util::copy_into(&mut c.photo_mime, b"url");
        c.photo_len = util::copy_into(&mut c.photo, v);
        return;
    }
    // 3.0 内联 Base64（ENCODING=b 或强制）
    let enc_b = has_type(params, b"b") || param_is_encoding_b(params) || base64_forced;
    if enc_b {
        let mime = param_type_value(params);
        if mime.len() > 0 {
            c.photo_mime_len = util::copy_into(&mut c.photo_mime, mime);
        } else {
            c.photo_mime_len = util::copy_into(&mut c.photo_mime, b"image/jpeg");
        }
        let n = util::base64_decode(v, &mut c.photo);
        c.photo_len = n.unwrap_or(0);
    } else if !v.is_empty() {
        c.photo_mime_len = util::copy_into(&mut c.photo_mime, b"url");
        c.photo_len = util::copy_into(&mut c.photo, v);
    }
}

fn param_is_encoding_b(params: &[u8]) -> bool {
    for part in params.split(|&c| c == b';') {
        let (k, v) = util::split2(part, b'=');
        if util::eq_ignore_case(k, b"ENCODING") && util::eq_ignore_case(util::trim(v), b"b") {
            return true;
        }
    }
    false
}

fn param_type_value(params: &[u8]) -> &[u8] {
    for part in params.split(|&c| c == b';') {
        let (k, v) = util::split2(part, b'=');
        if util::eq_ignore_case(k, b"TYPE") {
            return util::trim(v);
        }
        if util::eq_ignore_case(k, b"MEDIATYPE") {
            return util::trim(v);
        }
    }
    &[]
}

// ==================== vCard 解析 ====================

/// 解析一个 vCard 文本为 Contact。
/// `text` 会被原地展开（折叠消除），调用方需传入可变的 blob 缓冲。
pub fn parse_vcard(text: &mut [u8], len: usize, c: &mut Contact) -> bool {
    let new_len = unfold_in_place(text, len);
    let text = &text[..new_len];
    let mut props = [Prop { name: &[], params: &[], value: &[] }; 48];
    let n = parse_props(text, &mut props);

    let mut saw_fn = false;
    for i in 0..n {
        let p = &props[i];
        let name = p.name;
        // 属性组（4.0）：`item1.TEL` → 取最后一段作为属性名
        let name = match util::find_byte_from(name, b'.', 0) {
            Some(idx) => &name[idx + 1..],
            None => name,
        };
        if util::eq_ignore_case(name, b"VERSION") {
            continue;
        }
        if util::eq_ignore_case(name, b"FN") {
            c.fn_len = unescape_text(p.value, &mut c.fn_);
            saw_fn = true;
        } else if util::eq_ignore_case(name, b"N") {
            // N:family;given;additional;prefix;suffix
            let mut parts = p.value.split(|&c| c == b';');
            if let Some(fam) = parts.next() {
                c.n_family_len = unescape_text(util::trim(fam), &mut c.n_family);
            }
            if let Some(giv) = parts.next() {
                c.n_given_len = unescape_text(util::trim(giv), &mut c.n_given);
            }
        } else if util::eq_ignore_case(name, b"TEL") {
            push_tel(c, p.value, tel_kind(p.params));
        } else if util::eq_ignore_case(name, b"EMAIL") {
            push_email(c, p.value, email_kind(p.params));
        } else if util::eq_ignore_case(name, b"ORG") {
            c.org_len = unescape_text(p.value, &mut c.org);
        } else if util::eq_ignore_case(name, b"TITLE") {
            c.title_len = unescape_text(p.value, &mut c.title);
        } else if util::eq_ignore_case(name, b"NOTE") {
            c.note_len = unescape_text(p.value, &mut c.note);
        } else if util::eq_ignore_case(name, b"UID") {
            c.uid_len = util::copy_into(&mut c.uid, util::trim(p.value));
        } else if util::eq_ignore_case(name, b"PHOTO") {
            decode_photo(c, p.value, p.params, false);
        } else if util::eq_ignore_case(name, b"CATEGORIES") {
            push_group(c, p.value);
        } else if util::eq_ignore_case(name, b"MEMBER") {
            // 4.0：mailto:alice@example.com → 记入分组（成员关系）
            let v = util::trim(p.value);
            let v = if util::starts_with_ci(v, b"mailto:") { &v[7..] } else { v };
            if c.groups_len < MAX_GROUPS {
                c.groups[c.groups_len].name_len = util::copy_into(&mut c.groups[c.groups_len].name, v);
                c.groups_len += 1;
            }
        } else if util::eq_ignore_case(name, b"REV") {
            // 解析为 epoch 存 rrev？REV 是服务器修改时间，ETag 更可靠；忽略
        }
    }
    if !saw_fn && c.n_family_len == 0 && c.n_given_len == 0 {
        return false;
    }
    true
}

// ==================== vCard 生成 ====================

/// 生成 vCard 3.0 文本。返回总长度（不足则截断）。
pub fn vcard_to_text(c: &Contact, out: &mut [u8]) -> usize {
    let mut n = 0usize;
    n = prop_vcard(out, b"BEGIN", b"VCARD", n);
    n = prop_vcard(out, b"VERSION", b"3.0", n);
    n = prop_vcard(out, b"PRODID", b"-//KunxOS//KunxOS Sync//CN", n);
    if c.fn_len > 0 {
        let mut esc = [0u8; NAME_LEN * 2 + 8];
        let e = escape_text(&c.fn_[..c.fn_len], &mut esc);
        n = prop_vcard(out, b"FN", &esc[..e], n);
    }
    if c.n_family_len > 0 || c.n_given_len > 0 {
        let mut esc = [0u8; NAME_LEN * 2 + 8];
        let e = escape_text(&c.n_family[..c.n_family_len], &mut esc);
        let mut full = [0u8; NAME_LEN * 2 + 16];
        let mut f = util::copy_into(&mut full, &esc[..e]);
        f = util::push_str(&mut full, f, b";");
        let e = escape_text(&c.n_given[..c.n_given_len], &mut esc);
        f = util::push_str(&mut full, f, &esc[..e]);
        f = util::push_str(&mut full, f, b";;;");
        n = prop_vcard(out, b"N", &full[..f], n);
    }
    for i in 0..c.tels_len {
        let t = &c.tels[i];
        let ty: &[u8] = match t.kind {
            TEL_CELL => b"CELL",
            TEL_HOME => b"HOME",
            TEL_WORK => b"WORK",
            TEL_FAX => b"FAX",
            _ => b"VOICE",
        };
        let mut esc = [0u8; TEL_LEN * 2 + 8];
        let e = escape_text(&t.value[..t.value_len], &mut esc);
        let mut full = [0u8; TEL_LEN * 2 + 24];
        let mut f = util::copy_into(&mut full, b"TEL;TYPE=");
        f = util::push_str(&mut full, f, ty);
        n = prop_vcard(out, &full[..f], &esc[..e], n);
    }
    for i in 0..c.emails_len {
        let m = &c.emails[i];
        let ty: &[u8] = match m.kind {
            EMAIL_HOME => b"INTERNET,HOME",
            EMAIL_WORK => b"INTERNET,WORK",
            _ => b"INTERNET",
        };
        let mut esc = [0u8; EMAIL_LEN * 2 + 8];
        let e = escape_text(&m.value[..m.value_len], &mut esc);
        let mut full = [0u8; EMAIL_LEN * 2 + 24];
        let mut f = util::copy_into(&mut full, b"EMAIL;TYPE=");
        f = util::push_str(&mut full, f, ty);
        n = prop_vcard(out, &full[..f], &esc[..e], n);
    }
    if c.org_len > 0 {
        let mut esc = [0u8; NAME_LEN * 2 + 8];
        let e = escape_text(&c.org[..c.org_len], &mut esc);
        n = prop_vcard(out, b"ORG", &esc[..e], n);
    }
    if c.title_len > 0 {
        let mut esc = [0u8; NAME_LEN * 2 + 8];
        let e = escape_text(&c.title[..c.title_len], &mut esc);
        n = prop_vcard(out, b"TITLE", &esc[..e], n);
    }
    if c.note_len > 0 {
        let mut esc = [0u8; NOTE_LEN * 2 + 8];
        let e = escape_text(&c.note[..c.note_len], &mut esc);
        n = prop_vcard(out, b"NOTE", &esc[..e], n);
    }
    if c.photo_len > 0 {
        let is_url = c.photo_mime_len == 3 && util::eq_ignore_case(&c.photo_mime[..3], b"url");
        if is_url {
            n = prop_vcard(out, b"PHOTO;VALUE=URI", &c.photo[..c.photo_len], n);
        } else {
            let mime = if c.photo_mime_len > 0 { &c.photo_mime[..c.photo_mime_len] } else { b"JPEG" };
            let mut b64 = [0u8; PHOTO_LEN * 2 + 16];
            let b = util::base64_encode(&c.photo[..c.photo_len], &mut b64);
            let mut full = [0u8; PHOTO_LEN * 2 + 64];
            let mut f = util::copy_into(&mut full, b"PHOTO;ENCODING=b;TYPE=");
            f = util::push_str(&mut full, f, mime);
            f = util::push_str(&mut full, f, b":");
            n = util::push_str(out, n, &full[..f]);
            // 长行折叠（vCard 3.0：每 60 字节折行，续行以空格开头）
            let mut at = b.min(60);
            n = util::push_str(out, n, &b64[..at]);
            while at < b {
                let chunk = (b - at).min(60);
                n = util::push_str(out, n, b"\r\n ");
                n = util::push_str(out, n, &b64[at..at + chunk]);
                at += chunk;
            }
            n = util::push_str(out, n, b"\r\n");
        }
    }
    if c.groups_len > 0 {
        let mut full = [0u8; MAX_GROUPS * (GROUP_LEN + 2)];
        let mut f = 0usize;
        for i in 0..c.groups_len {
            if i > 0 {
                f = util::push_str(&mut full, f, b",");
            }
            f = util::push_str(&mut full, f, &c.groups[i].name[..c.groups[i].name_len]);
        }
        n = prop_vcard(out, b"CATEGORIES", &full[..f], n);
    }
    if c.uid_len > 0 {
        n = prop_vcard(out, b"UID", &c.uid[..c.uid_len], n);
    }
    n = prop_vcard(out, b"END", b"VCARD", n);
    n.min(out.len())
}

/// 追加 "NAME:value\r\n"。（结束标记为 CRLF 行尾）
fn prop_vcard(out: &mut [u8], name: &[u8], value: &[u8], at: usize) -> usize {
    let mut a = at;
    a = util::push_str(out, a, name);
    a = util::push_str(out, a, b":");
    a = util::push_str(out, a, value);
    a = util::push_str(out, a, b"\r\n");
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &[u8]) -> Contact {
        let mut buf = text.to_vec();
        let mut c = Contact::new();
        let bl = buf.len();
        assert!(parse_vcard(&mut buf, bl, &mut c));
        c
    }

    #[test]
    fn parse_vcard3_basic() {
        let c = parse(
            b"BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Alice Wang\r\n\
              N:Wang;Alice;;;\r\nTEL;TYPE=CELL:+8613800000001\r\n\
              TEL;TYPE=WORK:+862155500002\r\nEMAIL;TYPE=INTERNET,HOME:alice@example.com\r\n\
              ORG:KunxOS Inc\r\nEND:VCARD\r\n",
        );
        assert_eq!(&c.fn_[..c.fn_len], b"Alice Wang");
        assert_eq!(&c.n_family[..c.n_family_len], b"Wang");
        assert_eq!(&c.n_given[..c.n_given_len], b"Alice");
        assert_eq!(c.tels_len, 2);
        assert_eq!(&c.tels[0].value[..c.tels[0].value_len], b"+8613800000001");
        assert_eq!(c.tels[0].kind, TEL_CELL);
        assert_eq!(c.tels[1].kind, TEL_WORK);
        assert_eq!(c.emails_len, 1);
        assert_eq!(c.emails[0].kind, EMAIL_HOME);
        assert_eq!(&c.org[..c.org_len], b"KunxOS Inc");
    }

    #[test]
    fn parse_vcard4_groups_and_photo() {
        let c = parse(
            b"BEGIN:VCARD\r\nVERSION:4.0\r\nFN:Bob\r\nUID:bob-1\r\n\
              CATEGORIES:Friends,Work\r\n\
              PHOTO;MEDIATYPE=image/jpeg;ENCODING=b:aGVsbG8=\r\n\
              item1.TEL;TYPE=cell:+15550001\r\nEND:VCARD\r\n",
        );
        assert_eq!(c.groups_len, 2);
        assert_eq!(&c.groups[0].name[..c.groups[0].name_len], b"Friends");
        assert_eq!(&c.groups[1].name[..c.groups[1].name_len], b"Work");
        assert_eq!(&c.photo[..c.photo_len], b"hello");
        assert_eq!(c.tels_len, 1);
        assert_eq!(c.tels[0].kind, TEL_CELL);
    }

    #[test]
    fn parse_photo_url() {
        let c = parse(b"BEGIN:VCARD\r\nVERSION:4.0\r\nFN:X\r\nPHOTO:https://example.com/a.jpg\r\nEND:VCARD\r\n");
        assert_eq!(&c.photo_mime[..c.photo_mime_len], b"url");
        assert_eq!(&c.photo[..c.photo_len], b"https://example.com/a.jpg");
    }

    #[test]
    fn generate_roundtrip() {
        let mut c = Contact::new();
        c.uid_len = util::copy_into(&mut c.uid, b"c-1");
        c.fn_len = util::copy_into(&mut c.fn_, b"Zhang, San");
        c.n_family_len = util::copy_into(&mut c.n_family, b"Zhang");
        c.n_given_len = util::copy_into(&mut c.n_given, b"San");
        push_tel(&mut c, b"+8613800000001", TEL_CELL);
        push_email(&mut c, b"z@example.com", EMAIL_WORK);
        push_group(&mut c, b"Friends,Work");
        c.photo_mime_len = util::copy_into(&mut c.photo_mime, b"JPEG");
        c.photo_len = util::base64_decode(b"aGVsbG8gd29ybGQ=", &mut c.photo).unwrap();
        let mut out = [0u8; 2048];
        let n = vcard_to_text(&c, &mut out);
        let mut c2 = Contact::new();
        assert!(parse_vcard(&mut out, n, &mut c2));
        assert_eq!(&c2.fn_[..c2.fn_len], b"Zhang, San");
        assert_eq!(c2.tels_len, 1);
        assert_eq!(c2.tels[0].kind, TEL_CELL);
        assert_eq!(&c2.tels[0].value[..c2.tels[0].value_len], b"+8613800000001");
        assert_eq!(c2.emails_len, 1);
        assert_eq!(c2.emails[0].kind, EMAIL_WORK);
        assert_eq!(c2.groups_len, 2);
        assert_eq!(&c2.photo[..c2.photo_len], b"hello world");
    }

    #[test]
    fn folding_and_escaping() {
        let c = parse(
            b"BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Line1\\nLine2 with \\, comma\r\nEND:VCARD\r\n",
        );
        assert_eq!(&c.fn_[..c.fn_len], b"Line1\nLine2 with , comma");
    }
}
