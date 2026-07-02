use storage::btree::tree::{load_meta, BPlusTree};
use storage::buffer::BufferPool;
use storage::disk::DiskManager;
use storage::encoding::{decode_i64, encode_i64};

#[test]
fn kv_put_get_scan() {
    let dir = tempfile::tempdir().unwrap();
    let dm = DiskManager::open(dir.path().join("cli.db")).unwrap();
    let mut bp = BufferPool::new(dm, 16);
    let mut meta = load_meta(&mut bp).unwrap();
    let mut t = BPlusTree::open(&mut bp, &mut meta);
    t.insert(&encode_i64(3), b"three").unwrap();
    t.insert(&encode_i64(1), b"one").unwrap();
    let rows = t.scan(None, None).unwrap();
    let keys: Vec<i64> = rows.iter().map(|(k, _)| decode_i64(k)).collect();
    assert_eq!(keys, vec![1, 3]);
    t.checkpoint().unwrap();
}
