use crate::host;
use crate::sync::store::{Rec, Store};
use crate::sync::model::{FLAG_ON_SERVER, FLAG_DIRTY, FLAG_CONFLICT};
fn record(key: u8) -> Rec { let mut r=Rec::new(); r.key[0]=key; r.key_len=1; r }

#[test]
fn audit2_pending_remote_delete_survives_reload_and_compaction() {
    let mut fs=host::MemFs::new(); let mut buf=[0u8;64];
    let mut s=Store::new(&mut buf,b"idx",b"blob");
    let mut r=record(b'a'); r.flags=FLAG_ON_SERVER;
    assert!(s.upsert(r,b"remote event",false)); s.remove(0);
    assert!(s.save(&mut fs));
    let mut buf2=[0u8;64]; let mut reload=Store::new(&mut buf2,b"idx",b"blob");
    assert!(reload.load(&mut fs)); assert_eq!(reload.n,1);
    assert!(reload.recs[0].is_deleted() && reload.recs[0].is_dirty());
    assert!(reload.compact(&mut fs)); assert_eq!(reload.n,1);
    let mut ids=[0usize;4]; assert_eq!(reload.query_dirty(0,&mut ids),1);
    reload.recs[0].flags=(reload.recs[0].flags | FLAG_CONFLICT) & !FLAG_DIRTY;
    assert!(reload.compact(&mut fs)); assert_eq!(reload.n,1);
    assert_eq!(reload.query_dirty(0,&mut ids),0); // wait for conflict resolution
}
#[test]
fn audit2_recreate_deleted_record_is_visible() {
    let mut buf=[0u8;64]; let mut s=Store::new(&mut buf,b"idx",b"blob");
    assert!(s.upsert(record(b'a'),b"old",true)); s.remove(0);
    assert!(s.upsert(record(b'a'),b"new",true));
    assert_eq!(s.find(b"a"),Some(0)); assert!(!s.recs[0].is_deleted());
    assert_eq!(s.blob_of(0),Some(&b"new"[..])); assert!(s.recs[0].is_dirty());
}
#[test]
fn audit2_failed_insert_leaves_no_record_and_retry_succeeds() {
    let mut buf=[0u8;2]; let mut s=Store::new(&mut buf,b"idx",b"blob");
    assert!(!s.upsert(record(b'a'),b"too long",true));
    assert_eq!(s.n,0); assert_eq!(s.find(b"a"),None); assert_eq!(s.blob_len,0);
    assert!(s.upsert(record(b'a'),b"ok",true)); assert_eq!(s.blob_of(0),Some(&b"ok"[..]));
}
#[test]
fn audit2_failed_update_preserves_all_metadata_and_blob() {
    let mut buf=[0u8;4]; let mut s=Store::new(&mut buf,b"idx",b"blob");
    assert!(s.upsert(record(b'a'),b"old",false)); let before=s.recs[0];
    let mut r=record(b'a'); r.coll=7; r.mtime=42;
    assert!(!s.upsert(r,b"long-new-content",true));
    assert_eq!(s.recs[0].mtime,before.mtime); assert_eq!(s.recs[0].coll,before.coll);
    assert_eq!(s.recs[0].lrev,before.lrev); assert_eq!(s.recs[0].flags,before.flags);
    assert_eq!(s.blob_of(0),Some(&b"old"[..])); assert_eq!(s.blob_len,3);
}
#[test]
fn audit2_truncated_or_missing_blob_is_rejected() {
    let mut fs=host::MemFs::new(); let mut buf=[0u8;64];
    let mut s=Store::new(&mut buf,b"idx",b"blob");
    assert!(s.upsert(record(b'a'),b"record contents",true)); assert!(s.save(&mut fs));
    // 保存使用代际文件 blob.N：破坏当前已提交那一代的 blob。
    let blob_name=format!("blob.{}",s.active_slot().unwrap()).into_bytes();
    for contents in [Some(b"x".to_vec()),None] {
        if let Some(data)=contents { fs.files.insert(blob_name.clone(),data); }
        else { fs.files.remove(blob_name.as_slice()); }
        let mut buf2=[0u8;64]; let mut reload=Store::new(&mut buf2,b"idx",b"blob");
        assert!(!reload.load(&mut fs)); assert!(!reload.loaded); assert_eq!(reload.n,0);
    }
}
#[test]
fn audit2_local_delete_never_enters_outgoing_queue() {
    let mut buf=[0u8;64]; let mut s=Store::new(&mut buf,b"idx",b"blob");
    assert!(s.upsert(record(b'a'),b"local",true)); s.remove(0);
    assert_eq!(s.query_dirty(0,&mut [0usize;4]),0);
}
#[test]
fn audit2_empty_replacement_clears_previous_payload() {
    let mut buf=[0u8;64]; let mut s=Store::new(&mut buf,b"idx",b"blob");
    assert!(s.upsert(record(b'a'),b"old",true));
    assert!(s.upsert(record(b'a'),b"",true)); assert_eq!(s.recs[0].blen,0);
}
