use storage::disk::DiskManager;
use storage::page::{Page, PageId};

#[test]
fn allocate_write_read_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    let mut dm = DiskManager::open(&path).unwrap();
    let id = dm.allocate_page().unwrap();
    assert_eq!(id, PageId(0));
    let mut p = Page::new_zeroed();
    p.data_mut()[0..5].copy_from_slice(b"hello");
    dm.write_page(id, &mut p).unwrap();
    dm.sync().unwrap();

    let got = dm.read_page(id).unwrap();
    assert_eq!(&got.data()[0..5], b"hello");
    assert!(got.verify_checksum());
}

#[test]
fn read_out_of_range_errs() {
    let dir = tempfile::tempdir().unwrap();
    let mut dm = DiskManager::open(dir.path().join("t.db")).unwrap();
    assert!(matches!(
        dm.read_page(PageId(0)),
        Err(storage::StorageError::PageOutOfRange(0))
    ));
}
