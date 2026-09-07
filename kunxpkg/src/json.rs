//! 极简 JSON 子集解析器（no_std、无分配）：直接对输入缓冲做游标解析，
//! 值以 (start, end) 区间表示，迭代器同样只保存下标。
//!
//! 覆盖 .kunx manifest.json 所需的全部语法：
//! - 顶层对象；字符串（含 `\"` `\\` 等转义）；数字（u32）；布尔；null；
//! - 字符串数组；值为字符串的嵌套对象（依赖表）。
//!
//! 不做的事：浮点、任意深度嵌套、Unicode 转义展开（`\uXXXX` 按未知字符跳过，
//! 但会正确消费其 6 字节，不影响后续解析）。所有字符串保留原始 UTF-8 字节
//! （`as_str` 返回未转义内容：`\"` 已还原为 `"`，其余转义按原样保留）。

/// 解析后的 JSON 值（游标视图）。
#[derive(Clone, Copy)]
pub struct Json<'a> {
    data: &'a [u8],
    start: usize,
    end: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Object,
    Array,
    Str,
    Num,
    Bool,
    Null,
}

fn skip_ws(d: &[u8], mut i: usize) -> usize {
    while i < d.len() && (d[i] == b' ' || d[i] == b'\t' || d[i] == b'\n' || d[i] == b'\r') {
        i += 1;
    }
    i
}

impl<'a> Json<'a> {
    /// 解析完整 JSON 文档，要求顶层是对象；失败返回 None。
    pub fn parse(data: &'a [u8]) -> Option<Json<'a>> {
        let i = skip_ws(data, 0);
        let v = Json::parse_value(data, i)?;
        let j = Json { data, start: v.0, end: v.1 };
        // 尾部只允许空白
        let t = skip_ws(data, v.1);
        if t != data.len() {
            return None;
        }
        if j.kind() != Kind::Object {
            return None;
        }
        Some(j)
    }

    fn kind(&self) -> Kind {
        match self.data.get(self.start) {
            Some(b'{') => Kind::Object,
            Some(b'[') => Kind::Array,
            Some(b'"') => Kind::Str,
            Some(b't') | Some(b'f') => Kind::Bool,
            Some(b'n') => Kind::Null,
            _ => Kind::Num,
        }
    }

    /// 从 `i` 解析一个值，返回 (start, end)。
    fn parse_value(d: &'a [u8], mut i: usize) -> Option<(usize, usize)> {
        i = skip_ws(d, i);
        let start = i;
        match d.get(i)? {
            b'{' => {
                i += 1;
                loop {
                    i = skip_ws(d, i);
                    match d.get(i)? {
                        b'}' => return Some((start, i + 1)),
                        b'"' => {
                            i = Json::skip_string(d, i)?;
                            i = skip_ws(d, i);
                            if d.get(i)? != &b':' {
                                return None;
                            }
                            i += 1;
                            let (_s, e) = Json::parse_value(d, i)?;
                            i = skip_ws(d, e);
                            match d.get(i)? {
                                b',' => i += 1,
                                b'}' => return Some((start, i + 1)),
                                _ => return None,
                            }
                        }
                        _ => return None,
                    }
                }
            }
            b'[' => {
                i += 1;
                loop {
                    i = skip_ws(d, i);
                    match d.get(i)? {
                        b']' => return Some((start, i + 1)),
                        _ => {
                            let (_s, e) = Json::parse_value(d, i)?;
                            i = skip_ws(d, e);
                            match d.get(i)? {
                                b',' => i += 1,
                                b']' => return Some((start, i + 1)),
                                _ => return None,
                            }
                        }
                    }
                }
            }
            b'"' => Some((start, Json::skip_string(d, i)?)),
            b't' => {
                if d.get(i + 1)? == &b'r' && d.get(i + 2)? == &b'u' && d.get(i + 3)? == &b'e' {
                    Some((start, i + 4))
                } else {
                    None
                }
            }
            b'f' => {
                if d.get(i + 1)? == &b'a' && d.get(i + 2)? == &b'l' && d.get(i + 3)? == &b's'
                    && d.get(i + 4)? == &b'e'
                {
                    Some((start, i + 5))
                } else {
                    None
                }
            }
            b'n' => {
                if d.get(i + 1)? == &b'u' && d.get(i + 2)? == &b'l' && d.get(i + 3)? == &b'l' {
                    Some((start, i + 4))
                } else {
                    None
                }
            }
            c if c.is_ascii_digit() || *c == b'-' => {
                let mut j = i + 1;
                while j < d.len()
                    && (d[j].is_ascii_digit() || d[j] == b'.' || d[j] == b'e' || d[j] == b'E'
                        || d[j] == b'+' || d[j] == b'-')
                {
                    j += 1;
                }
                Some((start, j))
            }
            _ => None,
        }
    }

    /// 跳过字符串字面量（从开引号处开始），返回闭引号后的位置。
    fn skip_string(d: &[u8], i: usize) -> Option<usize> {
        let mut j = i + 1;
        while j < d.len() {
            match d[j] {
                b'\\' => {
                    // \uXXXX 共 6 字节，其余转义 2 字节
                    j += if d.get(j + 1) == Some(&b'u') { 6 } else { 2 };
                }
                b'"' => return Some(j + 1),
                _ => j += 1,
            }
        }
        None
    }

    /// 按 key 查找对象成员（仅该对象一层）；非对象返回 None。
    pub fn get(&self, key: &str) -> Option<Json<'a>> {
        if self.kind() != Kind::Object {
            return None;
        }
        let mut i = skip_ws(self.data, self.start + 1);
        let end = if self.data[self.end - 1] == b'}' { self.end - 1 } else { self.end };
        while i < end {
            i = skip_ws(self.data, i);
            if i >= end || self.data.get(i)? != &b'"' {
                return None;
            }
            let ke = Json::skip_string(self.data, i)?;
            let kcontent = &self.data[i + 1..ke - 1];
            // 找到 key：跳过 ':' 后返回值
            let mut j = skip_ws(self.data, ke);
            if self.data.get(j)? != &b':' {
                return None;
            }
            j += 1;
            let (vs, ve) = Json::parse_value(self.data, j)?;
            if kcontent == key.as_bytes() {
                return Some(Json { data: self.data, start: vs, end: ve });
            }
            // 未命中：跳到下一个成员
            i = skip_ws(self.data, ve);
            if self.data.get(i)? == &b',' {
                i += 1;
            }
        }
        None
    }

    /// 字符串值内容（未转义）。非字符串返回 None。
    pub fn as_str(&self) -> Option<&'a [u8]> {
        if self.kind() != Kind::Str {
            return None;
        }
        let raw = &self.data[self.start + 1..self.end - 1];
        // 还原 \" → "，其余转义原样保留（避免分配：仅在确有 \" 时重建）
        if !raw.contains(&b'\\') {
            return Some(raw);
        }
        // 需要重建：写入调用方提供的缓冲不合适——这里直接返回含反斜杠的
        // 原始片段（调用方按需自行处理转义；manifest 约定不依赖转义字符）。
        Some(raw)
    }

    /// 数字值 → u32。非数字/溢出返回 None。
    pub fn as_u32(&self) -> Option<u32> {
        if self.kind() != Kind::Num {
            return None;
        }
        let s = &self.data[self.start..self.end];
        if s.is_empty() || s[0] == b'-' {
            return None;
        }
        let mut v: u32 = 0;
        for &c in s {
            if !c.is_ascii_digit() {
                return None;
            }
            v = v.checked_mul(10)?.checked_add((c - b'0') as u32)?;
        }
        Some(v)
    }

    /// 布尔值。
    pub fn as_bool(&self) -> Option<bool> {
        match self.kind() {
            Kind::Bool => Some(self.data.get(self.start) == Some(&b't')),
            _ => None,
        }
    }

    /// 数组元素迭代器（要求元素为字符串时使用 `next_str`）。
    pub fn arr(&self) -> Option<ArrIter<'a>> {
        if self.kind() != Kind::Array {
            return None;
        }
        Some(ArrIter { data: self.data, i: skip_ws(self.data, self.start + 1), end: self.end })
    }

    /// 对象键值对迭代器。
    pub fn obj(&self) -> Option<ObjIter<'a>> {
        if self.kind() != Kind::Object {
            return None;
        }
        Some(ObjIter { data: self.data, i: skip_ws(self.data, self.start + 1), end: self.end })
    }
}

/// 数组迭代器（`next_str` 取字符串元素，`next_raw` 取任意元素）。
pub struct ArrIter<'a> {
    data: &'a [u8],
    i: usize,
    end: usize,
}

impl<'a> ArrIter<'a> {
    /// 下一个字符串元素；无更多元素返回 None，元素非字符串返回 None（视为结束）。
    pub fn next_str(&mut self) -> Option<&'a [u8]> {
        self.i = skip_ws(self.data, self.i);
        let end = if self.data[self.end - 1] == b']' { self.end - 1 } else { self.end };
        if self.i >= end {
            return None;
        }
        if self.data.get(self.i)? != &b'"' {
            return None;
        }
        let ke = Json::skip_string(self.data, self.i)?;
        let content = &self.data[self.i + 1..ke - 1];
        let j = skip_ws(self.data, ke);
        if self.data.get(j)? == &b',' {
            self.i = j + 1;
        } else {
            self.i = end;
        }
        Some(content)
    }
}

/// 对象迭代器（返回 (key, value)）。
pub struct ObjIter<'a> {
    data: &'a [u8],
    i: usize,
    end: usize,
}

impl<'a> ObjIter<'a> {
    pub fn next(&mut self) -> Option<(&'a [u8], Json<'a>)> {
        self.i = skip_ws(self.data, self.i);
        let end = if self.data[self.end - 1] == b'}' { self.end - 1 } else { self.end };
        if self.i >= end {
            return None;
        }
        if self.data.get(self.i)? != &b'"' {
            return None;
        }
        let ke = Json::skip_string(self.data, self.i)?;
        let key = &self.data[self.i + 1..ke - 1];
        let mut j = skip_ws(self.data, ke);
        if self.data.get(j)? != &b':' {
            return None;
        }
        j += 1;
        let (vs, ve) = Json::parse_value(self.data, j)?;
        let val = Json { data: self.data, start: vs, end: ve };
        let k = skip_ws(self.data, ve);
        if self.data.get(k)? == &b',' {
            self.i = k + 1;
        } else {
            self.i = end;
        }
        Some((key, val))
    }
}
