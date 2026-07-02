use storage::buffer::BufferPool;
use storage::disk::DiskManager;
use storage::page::PageId;

#[test]
fn fetch_caches_and_evicts() {
    let dir = tempfile::tempdir().unwrap();
    let mut dm = DiskManager::open(dir.path().join("t.db")).unwrap();
    for _ in 0..3 {
        dm.allocate_page().unwrap(); // pages 0,1,2 exist
    }
    let mut bp = BufferPool::new(dm, 2); // only 2 frames

    let f0 = bp.fetch(PageId(0)).unwrap();
    bp.frame_mut(f0).data_mut()[0] = 42;
    bp.mark_dirty(f0);
    bp.unpin(f0);

    let f1 = bp.fetch(PageId(1)).unwrap();
    bp.unpin(f1);
    // fetching page 2 must evict; the dirty page 0 flushes on the way out
    let _f2 = bp.fetch(PageId(2)).unwrap();

    let p = bp.disk_mut().read_page(PageId(0)).unwrap();
    assert_eq!(p.data()[0], 42);
}
