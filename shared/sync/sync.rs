//! 同步引擎：变更检测、增量同步、冲突解决、离线恢复。
//!
//! 设计（详见 docs/sync.md）：
//!   - **变更检测**：本地改动在写入存储时置 FLAG_DIRTY / FLAG_DELETED
//!     （即“同步队列”，按 lrev 排序）；远端改动经 RFC 6578 sync-collection
//!     增量报告（服务器不支持时回退 calendar/addressbook-query 全量对比）。
//!   - **推送**：创建用 PUT + If-None-Match:*；更新用 PUT + If-Match:<etag>
//!     （乐观并发）；删除用 DELETE。412 表示远端已被他人修改 → 冲突。
//!   - **拉取**：sync-collection 返回的变更 href 分批 multiget 取回，
//!     解析后按 ETag 更新本地；被删 href 删除本地（本地脏时按策略处理）。
//!   - **冲突解决策略**：RESOLVE_SERVER（服务器优先）/ RESOLVE_CLIENT
//!     （客户端优先）/ RESOLVE_MANUAL（标记冲突，保留本地，由 UI 决策）。
//!   - **离线支持**：任何网络失败 → 保留队列（脏标志），集合置
//!     needs_sync=true，返回错误；下次同步（应用启动 / 手动触发 /
//!     网络恢复探测）自动重试。
//!
//! 本模块不持有缓冲；resp / req / xml 由调用方提供（应用为静态缓冲）。

use crate::sync::accounts::{self, MetaStore};
use crate::sync::caldav;
use crate::sync::carddav;
use crate::sync::dav::{self, DavClient};
use crate::sync::fs::Fs;
use crate::sync::http::{self, HttpErr, Net};
use crate::sync::ical;
use crate::sync::model::*;
use crate::sync::store::{Rec, Store};
use crate::sync::util;
use crate::sync::vcard;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SyncErr {
    Net(HttpErr),
    NoAccount,
    Disabled,
    NoCollection,
    Store,
    OAuth2Unsupported,
    StoreFull,
}

impl SyncErr {
    pub fn label(&self) -> &'static [u8] {
        match self {
            SyncErr::Net(e) => e.label(),
            SyncErr::NoAccount => b"no_account",
            SyncErr::Disabled => b"disabled",
            SyncErr::NoCollection => b"no_collection",
            SyncErr::Store => b"store",
            SyncErr::OAuth2Unsupported => b"oauth2_unsupported",
            SyncErr::StoreFull => b"store_full",
        }
    }
}

/// 同步环境：文件系统 / 网络传输注入 + 设备密钥 + 冲突策略。
pub struct SyncEnv<'a> {
    pub fs: &'a mut dyn Fs,
    pub net: &'a mut dyn Net,
    pub dev_key: u32,
    pub now_us: u64,
    /// 冲突解决策略（RESOLVE_SERVER / RESOLVE_CLIENT / RESOLVE_MANUAL）。
    pub resolve: u8,
}

/// 一次 multiget 请求的最大 href 数。
const MULTIGET_BATCH: usize = 24;

/// 从集合路径 + UID 生成资源路径（如 /dav/cal/ + uid + .ics）。
pub fn make_href(coll_path: &[u8], uid: &[u8], ext: &[u8], out: &mut [u8]) -> usize {
    let mut n = util::copy_into(out, coll_path);
    if n > 0 && out[n - 1] != b'/' {
        n += util::copy_into(&mut out[n..], b"/");
    }
    n += util::copy_into(&mut out[n..], uid);
    n += util::copy_into(&mut out[n..], ext);
    n
}

// ==================== OAuth2 预留钩子 ====================

/// OAuth2 刷新令牌钩子（预留）。初始实现返回 None（不支持），
/// 后续在此接入令牌刷新流程（RFC 6749）。
pub fn oauth_refresh(_dev_key: u32, _meta: &MetaStore, _acct_id: u8) -> Option<[u8; 128]> {
    None
}

// ==================== 单集合同步 ====================

/// 同步一个集合（日历或地址簿）。
#[allow(clippy::too_many_arguments)]
pub fn sync_collection(
    env: &mut SyncEnv<'_>,
    meta: &mut MetaStore,
    coll_id: u8,
    store: &mut Store<'_>,
    kind: u8,
    resp: &mut [u8],
    req: &mut [u8],
    xml: &mut [u8],
    stats: &mut SyncStats,
) -> Result<(), SyncErr> {
    let coll = meta.collection(coll_id).ok_or(SyncErr::NoCollection)?;
    let acct_id = coll.acct;
    let acct = meta.account(acct_id).ok_or(SyncErr::NoAccount)?;
    if !acct.enabled {
        return Err(SyncErr::Disabled);
    }
    // 本地账户：无远端同步
    if acct.acct_type == ACCT_LOCAL {
        stats.set_msg(b"local");
        return Ok(());
    }
    // 认证
    let mut auth = [0u8; 260];
    let auth_len = match acct.auth {
        AUTH_OAUTH2 => {
            if !acct.has_refresh_token {
                return Err(SyncErr::OAuth2Unsupported);
            }
            // OAuth2 预留：令牌刷新后取 access token 作为 Bearer
            match oauth_refresh(env.dev_key, meta, acct_id) {
                Some(t) => {
                    let mut n = util::copy_into(&mut auth, b"Bearer ");
                    n += util::copy_into(&mut auth[n..], &t[..128]);
                    n
                }
                None => return Err(SyncErr::OAuth2Unsupported),
            }
        }
        _ => {
            let mut pw = [0u8; SECRET_LEN];
            let mut sn = [0u8; 16];
            let snl = accounts::MetaStore::account_secret_name(acct_id, &mut sn);
            let pl = meta.secret_get(&sn[..snl], env.dev_key, &mut pw);
            http::basic_auth(&acct.username[..acct.username_len], &pw[..pl], &mut auth)
        }
    };

    // 拆分 env 的可变借用：net 交给 DavClient，其余以值/重借用传入内部函数
    let fs: &mut dyn Fs = &mut *env.fs;
    let net: &mut dyn Net = &mut *env.net;
    let now_us = env.now_us;
    let resolve = env.resolve;

    let mut dav = DavClient::new(net, &acct.server[..acct.server_len], &auth[..auth_len]);

    // ---------- 推送本地改动（同步队列出队） ----------
    push_dirty(fs, now_us, resolve, &mut dav, meta, coll_id, store, kind, resp, req, stats)?;

    // ---------- 拉取远端改动 ----------
    pull_remote(fs, now_us, resolve, &mut dav, meta, coll_id, store, kind, resp, req, xml, stats)?;

    // ---------- 收尾 ----------
    if let Some(c) = meta.collection_mut(coll_id) {
        c.last_sync = now_us;
        c.needs_sync = false;
    }
    stats.ok = true;
    Ok(())
}

/// 推送阶段：处理所有脏记录（创建 / 更新 / 删除）。
fn push_dirty(
    fs: &mut dyn Fs,
    _now_us: u64,
    _resolve: u8,
    dav: &mut DavClient<'_>,
    meta: &mut MetaStore,
    coll_id: u8,
    store: &mut Store<'_>,
    kind: u8,
    resp: &mut [u8],
    req: &mut [u8],
    stats: &mut SyncStats,
) -> Result<(), SyncErr> {
    let mut dirty = [0usize; MAX_RECORDS];
    let n = store.query_dirty(coll_id, &mut dirty);
    for k in 0..n {
        let i = dirty[k];
        let rec = store.recs[i];
        if rec.key_len == 0 {
            continue;
        }
        if rec.is_deleted() {
            // 删除
            let st = match kind {
                COLL_CALENDAR => caldav::delete_event(dav, &rec.key[..rec.key_len], resp, req),
                _ => carddav::delete_contact(dav, &rec.key[..rec.key_len], resp, req),
            };
            match st {
                Ok(204) | Ok(200) | Ok(404) => {
                    // Keep it hidden until compaction, but remove it from the
                    // outgoing queue only after the server acknowledges DELETE.
                    store.recs[i].flags = (store.recs[i].flags | FLAG_DELETED)
                        & !(FLAG_DIRTY | FLAG_CONFLICT | FLAG_ON_SERVER);
                    store.recs[i].blen = 0;
                    stats.deleted += 1;
                }
                Ok(412) | Ok(409) => {
                    // 远端已被他人修改：标记冲突
                    store.recs[i].flags |= FLAG_CONFLICT;
                    store.recs[i].flags &= !FLAG_DIRTY;
                    stats.conflicts += 1;
                }
                Ok(_) => {
                    stats.errors += 1;
                    mark_needs_sync(meta, coll_id);
                }
                Err(e) => {
                    stats.fail(e.label());
                    mark_needs_sync(meta, coll_id);
                    return Err(SyncErr::Net(e));
                }
            }
        } else {
            // 创建 / 更新
            let data = match store.blob_of(i) {
                Some(d) => d,
                None => continue,
            };
            let created = rec.flags & FLAG_ON_SERVER == 0;
            let if_match = if created { &[][..] } else { rec.etag_slice() };
            // 创建：If-None-Match: *（防覆盖既有资源）；更新：仅 If-Match（乐观并发）
            let if_none: &[u8] = if created { b"*" } else { b"" };
            let mut etag_out = [0u8; ETAG_LEN];
            let st = match kind {
                COLL_CALENDAR => caldav::put_event(dav, &rec.key[..rec.key_len], data, if_match, if_none, &mut etag_out, resp, req),
                _ => carddav::put_contact(dav, &rec.key[..rec.key_len], data, if_match, if_none, &mut etag_out, resp, req),
            };
            match st {
                Ok(200) | Ok(201) | Ok(204) => {
                    store.recs[i].flags &= !(FLAG_DIRTY | FLAG_LOCAL_ONLY | FLAG_CONFLICT);
                    store.recs[i].flags |= FLAG_ON_SERVER;
                    store.recs[i].etag_len = util::copy_into(&mut store.recs[i].etag, &etag_out);
                    store.recs[i].rrev = store.recs[i].rrev.wrapping_add(1);
                    stats.pushed += 1;
                }
                Ok(412) => {
                    // 乐观并发失败：远端已变 → 冲突
                    store.recs[i].flags |= FLAG_CONFLICT;
                    stats.conflicts += 1;
                }
                Ok(_) => {
                    stats.errors += 1;
                    mark_needs_sync(meta, coll_id);
                }
                Err(e) => {
                    stats.fail(e.label());
                    mark_needs_sync(meta, coll_id);
                    return Err(SyncErr::Net(e));
                }
            }
        }
    }
    if !store.save(fs) {
        return Err(SyncErr::Store);
    }
    Ok(())
}

/// 拉取阶段：sync-collection 增量（或全量回退）+ multiget 取回。
fn pull_remote(
    fs: &mut dyn Fs,
    now_us: u64,
    resolve: u8,
    dav: &mut DavClient<'_>,
    meta: &mut MetaStore,
    coll_id: u8,
    store: &mut Store<'_>,
    kind: u8,
    resp: &mut [u8],
    req: &mut [u8],
    xml: &mut [u8],
    stats: &mut SyncStats,
) -> Result<(), SyncErr> {
    let (path_arr, path_len, tok_arr, tok_len) = {
        let coll = meta.collection(coll_id).ok_or(SyncErr::NoCollection)?;
        (coll.path, coll.path_len, coll.sync_token, coll.sync_token_len)
    };
    let path = &path_arr[..path_len];
    let token = &tok_arr[..tok_len];
    let _ = (now_us, resolve);

    let delta = if kind == COLL_CALENDAR {
        let mut d = caldav::SyncDelta::new();
        caldav::sync_collection(dav, path, token, &mut d, resp, req, xml).map_err(SyncErr::Net)?;
        d
    } else {
        let mut d = carddav::SyncDelta::new();
        carddav::sync_collection(dav, path, token, &mut d, resp, req, xml).map_err(SyncErr::Net)?;
        d
    };

    if delta.supported && delta.token_len > 0 {
        // 增量路径
        apply_changed(fs, now_us, resolve, dav, meta, coll_id, store, kind, path, &delta.changed, &delta.changed_len, delta.n_changed, resp, req, xml, stats)?;
        apply_deleted(resolve, meta, coll_id, store, &delta.deleted, &delta.deleted_len, delta.n_deleted, stats);
        if let Some(c) = meta.collection_mut(coll_id) {
            c.sync_token_len = util::copy_into(&mut c.sync_token, &delta.token[..delta.token_len]);
        }
    } else {
        // 回退：全量查询对比（calendar-query 窗口 / addressbook-query）
        full_refresh(fs, now_us, resolve, dav, meta, coll_id, store, kind, resp, req, xml, stats)?;
        if let Some(c) = meta.collection_mut(coll_id) {
            if delta.token_len > 0 {
                c.sync_token_len = util::copy_into(&mut c.sync_token, &delta.token[..delta.token_len]);
            } else {
                c.sync_token_len = 0;
            }
        }
    }
    Ok(())
}

/// 全量回退：枚举远端 (href, etag)，与本地对比。
fn full_refresh(
    fs: &mut dyn Fs,
    now_us: u64,
    resolve: u8,
    dav: &mut DavClient<'_>,
    meta: &mut MetaStore,
    coll_id: u8,
    store: &mut Store<'_>,
    kind: u8,
    resp: &mut [u8],
    req: &mut [u8],
    xml: &mut [u8],
    stats: &mut SyncStats,
) -> Result<(), SyncErr> {
    let (path_arr, path_len) = {
        let coll = meta.collection(coll_id).ok_or(SyncErr::NoCollection)?;
        (coll.path, coll.path_len)
    };
    let path = &path_arr[..path_len];
    // 收集远端清单
    let mut hits = [[0u8; KEY_LEN]; dav::MAX_ITEMS];
    let mut hit_etag = [[0u8; ETAG_LEN]; dav::MAX_ITEMS];
    let mut hit_len = [0usize; dav::MAX_ITEMS];
    let mut etag_len = [0usize; dav::MAX_ITEMS];
    let mut nh = 0usize;

    let mut collect = |href: &[u8], etag: &[u8]| -> bool {
        if nh < dav::MAX_ITEMS {
            hit_len[nh] = util::copy_into(&mut hits[nh], href);
            etag_len[nh] = util::copy_into(&mut hit_etag[nh], etag);
            nh += 1;
            true
        } else {
            false
        }
    };
    if kind == COLL_CALENDAR {
        // 实用窗口：过去 1 年 ~ 未来 2 年（文档说明局限）
        let now = (now_us / 1_000_000) as i64;
        caldav::calendar_query(dav, path, now - 366 * 86400, now + 730 * 86400, resp, req, xml, &mut collect).map_err(SyncErr::Net)?;
    } else {
        carddav::addressbook_query(dav, path, resp, req, xml, &mut collect).map_err(SyncErr::Net)?;
    }

    // 远端缺失 → 本地删除（仅非脏、服务器已知）
    let mut local = [0usize; MAX_RECORDS];
    let nl = store.query_coll(coll_id, &mut local);
    for k in 0..nl {
        let i = local[k];
        let rec = store.recs[i];
        if rec.flags & FLAG_ON_SERVER == 0 || rec.is_dirty() {
            continue;
        }
        let found = (0..nh).any(|h| &hits[h][..hit_len[h]] == rec.key_slice());
        if !found {
            store.recs[i].flags |= FLAG_DELETED;
            store.recs[i].blen = 0;
            stats.deleted += 1;
        }
    }
    // 远端新增 / 变更 → multiget
    let mut changed = [[0u8; KEY_LEN]; caldav::MAX_SYNC_ITEMS];
    let mut changed_len = [0usize; caldav::MAX_SYNC_ITEMS];
    let mut nc = 0usize;
    for h in 0..nh {
        let key = &hits[h][..hit_len[h]];
        let remote_etag = &hit_etag[h][..etag_len[h]];
        match store.find_any(key) {
            None => {
                if nc < dav::MAX_ITEMS {
                    changed_len[nc] = util::copy_into(&mut changed[nc], key);
                    nc += 1;
                }
            }
            Some(i) => {
                let rec = store.recs[i];
                if rec.etag_slice() != remote_etag && !rec.is_dirty() {
                    if nc < dav::MAX_ITEMS {
                        changed_len[nc] = util::copy_into(&mut changed[nc], key);
                        nc += 1;
                    }
                } else if rec.etag_slice() != remote_etag && rec.is_dirty() {
                    let _ = handle_conflict(resolve, store, i, remote_etag, stats);
                }
            }
        }
    }
    apply_changed(fs, now_us, resolve, dav, meta, coll_id, store, kind, path, &changed, &changed_len, nc, resp, req, xml, stats)?;
    store.save(fs).then_some(()).ok_or(SyncErr::Store)
}

/// 处理变更 href：分批 multiget → 解析 → 本地更新。
fn apply_changed(
    _fs: &mut dyn Fs,
    now_us: u64,
    resolve: u8,
    dav: &mut DavClient<'_>,
    _meta: &mut MetaStore,
    coll_id: u8,
    store: &mut Store<'_>,
    kind: u8,
    coll_path: &[u8],
    changed: &[[u8; KEY_LEN]; caldav::MAX_SYNC_ITEMS],
    changed_len: &[usize; caldav::MAX_SYNC_ITEMS],
    n: usize,
    resp: &mut [u8],
    req: &mut [u8],
    xml: &mut [u8],
    stats: &mut SyncStats,
) -> Result<(), SyncErr> {
    let mut batch = 0usize;
    while batch < n {
        let end = (batch + MULTIGET_BATCH).min(n);
        // 组装 href 引用数组
        let mut hrefs: [&[u8]; MULTIGET_BATCH] = [&[]; MULTIGET_BATCH];
        let mut hn = 0usize;
        for k in batch..end {
            hrefs[hn] = &changed[k][..changed_len[k]];
            hn += 1;
        }
        let res = if kind == COLL_CALENDAR {
            caldav::calendar_multiget(dav, coll_path, &hrefs[..hn], resp, req, xml, |href, data, etag| {
                apply_one(now_us, resolve, store, coll_id, kind, href, data, etag, stats);
                true
            })
        } else {
            carddav::addressbook_multiget(dav, coll_path, &hrefs[..hn], resp, req, xml, |href, data, etag| {
                apply_one(now_us, resolve, store, coll_id, kind, href, data, etag, stats);
                true
            })
        };
        if let Err(e) = res {
            stats.fail(e.label());
            return Err(SyncErr::Net(e));
        }
        batch = end;
    }
    Ok(())
}

/// 应用单个远端资源到本地存储。
fn apply_one(
    now_us: u64,
    resolve: u8,
    store: &mut Store<'_>,
    coll_id: u8,
    kind: u8,
    href: &[u8],
    data: &[u8],
    etag: &[u8],
    stats: &mut SyncStats,
) {
    // 解析数据（就地展开，需要可变缓冲）
    let mut buf = [0u8; 8192];
    let bl = util::copy_into(&mut buf, data);
    let mut rec = Rec::new();
    rec.key_len = util::copy_into(&mut rec.key, href);
    rec.coll = coll_id;
    rec.flags = FLAG_ON_SERVER;
    rec.mtime = now_us;
    rec.etag_len = util::copy_into(&mut rec.etag, etag);
    let parsed_ok = if kind == COLL_CALENDAR {
        let mut ev = Event::new();
        if ical::parse_event(&mut buf, bl, &mut ev) {
            rec.seq = ev.seq;
            true
        } else {
            false
        }
    } else {
        let mut c = Contact::new();
        vcard::parse_vcard(&mut buf, bl, &mut c)
    };
    if !parsed_ok {
        stats.errors += 1;
        return;
    }
    // 冲突检查：本地同 key 存在且脏 / 已冲突
    if let Some(i) = store.find_any(href) {
        let r = store.recs[i];
        if r.is_dirty() || r.is_conflict() {
            let apply_remote = handle_conflict(resolve, store, i, etag, stats);
            if !apply_remote {
                return; // 客户端优先 / 手动：保留本地，不覆盖
            }
            // 服务器优先：本地版本作废，继续应用远端（rev 递增）。
            // ★ 本地若带着墓碑（删除推送遇非网络错误未清除），必须先摘掉
            // DELETED：否则 upsert 保留旧 flags → 远端重建的资源本地不可见，
            // 且 query_dirty 下轮再发一次 DELETE 把服务器刚重建的数据真删。
            store.recs[i].flags &= !FLAG_DELETED;
            rec.rrev = r.rrev.wrapping_add(1);
        } else {
            rec.rrev = r.rrev.wrapping_add(1);
        }
    }
    let ok = store.upsert(rec, &buf[..bl], false);
    if ok {
        stats.pulled += 1;
    } else {
        stats.errors += 1;
    }
}

/// 冲突处理（按策略）。返回是否应继续应用远端版本
/// （仅 RESOLVE_SERVER 返回 true）。
fn handle_conflict(resolve: u8, store: &mut Store<'_>, i: usize, remote_etag: &[u8], stats: &mut SyncStats) -> bool {
    match resolve {
        RESOLVE_SERVER => {
            // 服务器优先：本地改动作废，应用远端版本
            store.recs[i].flags &= !(FLAG_DIRTY | FLAG_CONFLICT);
            stats.conflicts += 1;
            true
        }
        RESOLVE_CLIENT => {
            // 客户端优先：保持 DIRTY，记录远端 etag 使下次 If-Match 成功
            store.recs[i].flags &= !FLAG_CONFLICT;
            store.recs[i].flags |= FLAG_DIRTY;
            if !remote_etag.is_empty() {
                store.recs[i].etag_len = util::copy_into(&mut store.recs[i].etag, remote_etag);
            }
            stats.conflicts += 1;
            false
        }
        _ => {
            // 手动：标记冲突，保留本地；etag 记录远端（供解决时 If-Match）
            store.recs[i].flags |= FLAG_CONFLICT;
            if !remote_etag.is_empty() {
                store.recs[i].etag_len = util::copy_into(&mut store.recs[i].etag, remote_etag);
            }
            stats.conflicts += 1;
            false
        }
    }
}

/// 处理远端删除。
fn apply_deleted(
    resolve: u8,
    _meta: &mut MetaStore,
    coll_id: u8,
    store: &mut Store<'_>,
    deleted: &[[u8; KEY_LEN]; caldav::MAX_SYNC_ITEMS],
    deleted_len: &[usize; caldav::MAX_SYNC_ITEMS],
    n: usize,
    stats: &mut SyncStats,
) {
    for k in 0..n {
        let key = &deleted[k][..deleted_len[k]];
        if let Some(i) = store.find_any(key) {
            let rec = store.recs[i];
            if rec.is_dirty() {
                // 双方删除 / 本地有改动：按策略
                if handle_conflict(resolve, store, i, &[][..], stats) {
                    // 服务器优先：本地改动让位 → 按远端删除落地（否则记录
                    // 不 dirty 也未删，增量路径不再覆盖，永久分歧）。
                    store.recs[i].flags |= FLAG_DELETED;
                    store.recs[i].blen = 0;
                    stats.deleted += 1;
                }
            } else {
                store.recs[i].flags |= FLAG_DELETED;
                store.recs[i].blen = 0;
                stats.deleted += 1;
            }
        }
        let _ = coll_id;
    }
}

fn mark_needs_sync(meta: &mut MetaStore, coll_id: u8) {
    if let Some(c) = meta.collection_mut(coll_id) {
        c.needs_sync = true;
    }
}

// ==================== 冲突解决 API ====================

/// 手动解决冲突：keep=RESOLVE_SERVER（丢弃本地，下次拉取覆盖）或
/// RESOLVE_CLIENT（保留本地，下次推送覆盖服务器）。返回 false 表示
/// 无此记录或未处于冲突态。
pub fn resolve_conflict(env: &mut SyncEnv<'_>, store: &mut Store<'_>, i: usize, keep: u8) -> bool {
    if i >= store.n || !store.recs[i].is_conflict() {
        return false;
    }
    match keep {
        RESOLVE_SERVER => {
            store.recs[i].flags &= !(FLAG_DIRTY | FLAG_CONFLICT);
        }
        _ => {
            store.recs[i].flags &= !FLAG_CONFLICT;
            store.recs[i].flags |= FLAG_DIRTY;
        }
    }
    store.save(env.fs)
}

// ==================== 同步全部集合 ====================
/// 同步某类型（日历 / 地址簿）的所有启用集合。返回是否有任何成功。
pub fn sync_all(
    env: &mut SyncEnv<'_>,
    meta: &mut MetaStore,
    store: &mut Store<'_>,
    kind: u8,
    resp: &mut [u8],
    req: &mut [u8],
    xml: &mut [u8],
    stats: &mut SyncStats,
) -> Result<(), SyncErr> {
    // 收集该类型集合 id
    let mut colls = [0u8; 16];
    let mut nc = 0usize;
    for i in 0..meta.n_colls {
        let c = meta.colls[i];
        if c.kind == kind && c.acct != 0 {
            if nc < colls.len() {
                colls[nc] = c.id;
                nc += 1;
            }
        }
    }
    let mut any_ok = false;
    let mut first_err = Ok(());
    for k in 0..nc {
        let mut sub = SyncStats::new();
        let r = sync_collection(env, meta, colls[k], store, kind, resp, req, xml, &mut sub);
        match r {
            Ok(()) => {
                any_ok = true;
                stats.pushed += sub.pushed;
                stats.pulled += sub.pulled;
                stats.deleted += sub.deleted;
                stats.conflicts += sub.conflicts;
                stats.errors += sub.errors;
            }
            Err(e) => {
                if first_err.is_ok() {
                    first_err = Err(e);
                }
                stats.errors += 1;
            }
        }
    }
    if any_ok {
        stats.ok = true;
        Ok(())
    } else {
        stats.ok = false;
        first_err
    }
}

/// 是否还有待同步的本地改动（应用启动时判断是否需要自动同步）。
pub fn has_pending(store: &Store<'_>, coll_id: u8) -> bool {
    let mut out = [0usize; 8];
    store.query_dirty(coll_id, &mut out) > 0
}

// ==================== 本地创建 ====================

/// 本地新建事件：生成 UID / href，写入 iCalendar Blob，置脏。
/// 返回记录的 Blob（供 UI 确认）。成功返回 true。
pub fn create_event_local(
    env: &mut SyncEnv<'_>,
    store: &mut Store<'_>,
    coll_id: u8,
    coll_path: &[u8],
    ev: &mut Event,
    counter: u32,
) -> bool {
    let mut uid = [0u8; UID_LEN];
    let ul = util::gen_uid(env.now_us, counter, env.dev_key, &mut uid);
    ev.uid_len = ul;
    ev.uid[..ul].copy_from_slice(&uid[..ul]);
    let mut href = [0u8; KEY_LEN];
    let hl = make_href(coll_path, &uid[..ul], b".ics", &mut href);
    let mut ical_buf = [0u8; 4096];
    let il = ical::event_to_ical(ev, &mut ical_buf);
    let mut rec = Rec::new();
    rec.key_len = util::copy_into(&mut rec.key, &href[..hl]);
    rec.coll = coll_id;
    rec.flags = FLAG_DIRTY | FLAG_LOCAL_ONLY;
    rec.mtime = env.now_us;
    rec.seq = 0;
    store.upsert(rec, &ical_buf[..il], true)
}

/// 本地新建联系人：生成 UID / href，写入 vCard Blob，置脏。
pub fn create_contact_local(
    env: &mut SyncEnv<'_>,
    store: &mut Store<'_>,
    coll_id: u8,
    coll_path: &[u8],
    c: &mut Contact,
    counter: u32,
) -> bool {
    let mut uid = [0u8; UID_LEN];
    let ul = util::gen_uid(env.now_us, counter, env.dev_key, &mut uid);
    c.uid_len = ul;
    c.uid[..ul].copy_from_slice(&uid[..ul]);
    let mut href = [0u8; KEY_LEN];
    let hl = make_href(coll_path, &uid[..ul], b".vcf", &mut href);
    let mut vcard_buf = [0u8; 8192];
    let vl = vcard::vcard_to_text(c, &mut vcard_buf);
    let mut rec = Rec::new();
    rec.key_len = util::copy_into(&mut rec.key, &href[..hl]);
    rec.coll = coll_id;
    rec.flags = FLAG_DIRTY | FLAG_LOCAL_ONLY;
    rec.mtime = env.now_us;
    store.upsert(rec, &vcard_buf[..vl], true)
}

// —— 小工具 ——
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn href_builder() {
        let mut out = [0u8; 128];
        let n = make_href(b"/dav/cal/", b"uid-1", b".ics", &mut out);
        assert_eq!(&out[..n], b"/dav/cal/uid-1.ics");
        let n = make_href(b"/dav/cal", b"uid-1", b".ics", &mut out);
        assert_eq!(&out[..n], b"/dav/cal/uid-1.ics");
    }
}
