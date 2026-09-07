//! 增量索引补丁（KUNXDELTA1）应用：纯函数，设备端与主机端共用。
/// 索引缓冲上限（与 repo.rs 一致）。
const INDEX_MAX: usize = 16384;

fn next_line(b: &[u8], from: usize) -> usize {
    let mut i = from;
    while i < b.len() && b[i] != b'\n' {
        i += 1;
    }
    if i < b.len() {
        i + 1
    } else {
        i
    }
}

fn trim(b: &[u8]) -> &[u8] {
    let mut s = 0usize;
    let mut e = b.len();
    while s < e && (b[s] == b' ' || b[s] == b'\t' || b[s] == b'\r' || b[s] == b'\n') {
        s += 1;
    }
    while e > s && (b[e - 1] == b' ' || b[e - 1] == b'\t' || b[e - 1] == b'\r' || b[e - 1] == b'\n') {
        e -= 1;
    }
    &b[s..e]
}



/// 增量补丁格式（UTF-8 文本）：
/// ```text
/// KUNXDELTA1
/// Base: <rev>
/// Target: <rev>
///
/// A <Package: name>          # 新增/替换记录（直到空行）
/// Package: ...
/// Version: ...
///
/// D <Package: name>          # 删除记录
/// ```
/// 应用后写入 `out`，返回长度；失败 None。
pub fn apply_delta(delta: &[u8], base: &[u8], out: &mut [u8]) -> Option<usize> {
    if !delta.starts_with(b"KUNXDELTA1\n") {
        return None;
    }
    // 跳过头部到第一个空行
    let mut body_start = 0usize;
    let mut ls = 0usize;
    let mut found_blank = false;
    while ls < delta.len() {
        let le = next_line(delta, ls);
        let line = trim(&delta[ls..le]);
        if line.is_empty() {
            body_start = le;
            found_blank = true;
            break;
        }
        ls = le;
    }
    if !found_blank {
        return None;
    }
    // 逐条操作应用
    let mut cur = [0u8; INDEX_MAX];
    let cur_len = base.len().min(cur.len());
    cur[..cur_len].copy_from_slice(&base[..cur_len]);
    let mut cur_len = cur_len;

    let mut ls = body_start;
    while ls < delta.len() {
        let le = next_line(delta, ls);
        let line = trim(&delta[ls..le]);
        ls = le;
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix(b"A ") {
            // 收集新记录（到空行）
            let mut rec = [0u8; 4096];
            let mut rn = 0usize;
            let mut started = false;
            while ls < delta.len() {
                let le2 = next_line(delta, ls);
                let l2 = trim(&delta[ls..le2]);
                ls = le2;
                if l2.is_empty() {
                    if started {
                        break;
                    }
                    continue;
                }
                started = true;
                for &c in l2 {
                    if rn < rec.len() {
                        rec[rn] = c;
                        rn += 1;
                    }
                }
                if rn < rec.len() {
                    rec[rn] = b'\n';
                    rn += 1;
                }
            }
            if !started || rn == 0 {
                return None;
            }
            // 替换/追加记录
            cur_len = replace_record(&cur[..cur_len], name, &rec[..rn], out)?;
            let c = cur_len.min(cur.len());
            cur[..c].copy_from_slice(&out[..c]);
            cur_len = c;
        } else if let Some(name) = line.strip_prefix(b"D ") {
            cur_len = remove_record(&cur[..cur_len], name, out)?;
            let c = cur_len.min(cur.len());
            cur[..c].copy_from_slice(&out[..c]);
            cur_len = c;
        } else {
            return None;
        }
    }
    let c = cur_len.min(out.len());
    out[..c].copy_from_slice(&cur[..c]);
    Some(c)
}

/// 在索引中定位名为 `name` 的记录（Package: 行），返回 (start, end)。
/// end 指向记录后的空行（含）。
pub fn find_record(idx: &[u8], name: &[u8]) -> Option<(usize, usize)> {
    let mut ls = 0usize;
    while ls < idx.len() {
        let le = next_line(idx, ls);
        let line = trim(&idx[ls..le]);
        if line.len() >= 8 && &line[..8] == b"Package:" {
            let rec_name = trim(&line[8..]);
            let rs = ls;
            let mut r = le;
            let mut end = le;
            while r < idx.len() {
                let re = next_line(idx, r);
                let rl = trim(&idx[r..re]);
                if rl.is_empty() || (rl.len() >= 8 && &rl[..8] == b"Package:") {
                    end = r;
                    break;
                }
                end = re;
                r = re;
            }
            if rec_name == name {
                return Some((rs, end));
            }
        }
        ls = le;
    }
    None
}

/// 替换（或追加）名为 `name` 的记录，写入 out，返回新长度。
/// 记录以空行分隔；new_rec 每行以 `\n` 结尾（含最后一行）。
fn replace_record(idx: &[u8], name: &[u8], new_rec: &[u8], out: &mut [u8]) -> Option<usize> {
    match find_record(idx, name) {
        Some((s, e)) => {
            let mut n = 0usize;
            n += copy_bounded(out, n, &idx[..s]);
            n += copy_bounded(out, n, new_rec);
            // 若原记录后无空行分隔（[e..] 不以 `\n` 开头），补一个空行
            if e >= idx.len() || idx[e] != b'\n' {
                if n < out.len() {
                    out[n] = b'\n';
                    n += 1;
                }
            }
            n += copy_bounded(out, n, &idx[e..]);
            Some(n)
        }
        None => {
            let mut n = 0usize;
            n += copy_bounded(out, n, idx);
            // 保证新记录前有空行分隔（记录以 `\n` 结尾，空行 = 再补 `\n`）
            let ends_nl = n > 0 && out[n - 1] == b'\n';
            let has_blank = n > 1 && out[n - 1] == b'\n' && out[n - 2] == b'\n';
            if !ends_nl && n < out.len() {
                out[n] = b'\n';
                n += 1;
            }
            if !has_blank && n < out.len() {
                out[n] = b'\n';
                n += 1;
            }
            n += copy_bounded(out, n, new_rec);
            Some(n)
        }
    }
}

/// 删除名为 `name` 的记录，写入 out，返回新长度。
fn remove_record(idx: &[u8], name: &[u8], out: &mut [u8]) -> Option<usize> {
    let (s, e) = find_record(idx, name)?;
    let mut n = 0usize;
    n += copy_bounded(out, n, &idx[..s]);
    n += copy_bounded(out, n, &idx[e..]);
    Some(n)
}

fn copy_bounded(out: &mut [u8], off: usize, data: &[u8]) -> usize {
    let c = data.len().min(out.len().saturating_sub(off));
    out[off..off + c].copy_from_slice(&data[..c]);
    c
}

