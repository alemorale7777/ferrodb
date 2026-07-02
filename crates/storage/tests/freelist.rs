use storage::disk::DiskManager;
use storage::freelist;
use storage::meta::MetaPage;

#[test]
fn freed_page_is_reused_lifo() {
    let dir = tempfile::tempdir().unwrap();
    let mut dm = DiskManager::open(dir.path().join("t.db")).unwrap();
    let mut meta = MetaPage {
        magic: MetaPage::MAGIC,
        version: 1,
        free_list_head: None,
        tree_root: None,
    };

    let a = freelist::alloc(&mut dm, &mut meta).unwrap(); // fresh
    let b = freelist::alloc(&mut dm, &mut meta).unwrap(); // fresh
    freelist::free(&mut dm, &mut meta, a).unwrap();
    let c = freelist::alloc(&mut dm, &mut meta).unwrap(); // reuse `a`
    assert_eq!(c, a);
    assert_ne!(b, c);
    assert_eq!(meta.free_list_head, None);
}
