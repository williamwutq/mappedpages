use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::meta::{FIRST_DATA_PAGE, MAGIC, MetaPage, MetaSelector, Superblock};
use crate::{MappedPageError, PageId, Pager};

// ── Temp-file helper ──────────────────────────────────────────────────────────

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempPath(PathBuf);

impl TempPath {
    fn new() -> Self {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!("mppg_test_{}_{}", std::process::id(), id);
        TempPath(std::env::temp_dir().join(name))
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

// ── File-corruption helpers ───────────────────────────────────────────────────

fn write_at(path: &Path, offset: u64, data: &[u8]) {
    let mut f = fs::OpenOptions::new().write(true).open(path).unwrap();
    f.seek(SeekFrom::Start(offset)).unwrap();
    f.write_all(data).unwrap();
    f.flush().unwrap();
}

fn zero_range(path: &Path, offset: u64, len: usize) {
    write_at(path, offset, &vec![0u8; len]);
}

// Byte offset of the embedded checksum at the end of a metadata page.
fn meta_checksum_offset(sel: MetaSelector, page_size: usize) -> u64 {
    sel.page_id() * page_size as u64 + page_size as u64 - 4
}

// ── MetaPage unit tests ───────────────────────────────────────────────────────

#[test]
fn meta_new_marks_reserved_pages() {
    let m = MetaPage::new_for_capacity(8);
    assert_eq!(m.total_pages, 8);
    assert_eq!(m.free_count, 5); // 8 total − 3 reserved
    assert_eq!(m.generation, 0);
    // bits 0, 1, 2 set (reserved); bit 3 clear (free)
    assert_eq!(m.bitmap[0] & 0b00001111, 0b00000111);
}

#[test]
fn meta_alloc_returns_first_free() {
    let mut m = MetaPage::new_for_capacity(8);
    assert_eq!(m.alloc_page(), Some(3));
    assert_eq!(m.free_count, 4);
}

#[test]
fn meta_alloc_sequential_order() {
    let mut m = MetaPage::new_for_capacity(8);
    let ids: Vec<u64> = (0..5).map(|_| m.alloc_page().unwrap()).collect();
    assert_eq!(ids, [3, 4, 5, 6, 7]);
    assert_eq!(m.free_count, 0);
}

#[test]
fn meta_alloc_exhaustion_returns_none() {
    let mut m = MetaPage::new_for_capacity(8);
    for _ in 0..5 {
        m.alloc_page().unwrap();
    }
    assert!(m.alloc_page().is_none());
    assert_eq!(m.free_count, 0);
}

// total_pages not a multiple of 8: bits beyond the last valid page must not be
// set when alloc returns None (the bitmap corruption bug that was fixed).
#[test]
fn meta_alloc_bitmap_boundary_no_corruption() {
    let mut m = MetaPage::new_for_capacity(5); // pages 0-4; bits 5-7 are padding
    assert_eq!(m.alloc_page(), Some(3));
    assert_eq!(m.alloc_page(), Some(4));
    assert!(m.alloc_page().is_none()); // exhausted

    // padding bits (5, 6, 7) must stay 0
    assert_eq!(m.bitmap[0] & 0b1110_0000, 0);
    assert_eq!(m.free_count, 0);
}

#[test]
fn meta_free_then_realloc() {
    let mut m = MetaPage::new_for_capacity(8);
    m.alloc_page(); // page 3
    assert!(m.free_page(3));
    assert_eq!(m.free_count, 5);
    assert_eq!(m.alloc_page(), Some(3)); // reused
}

#[test]
fn meta_double_free_returns_false_no_undercount() {
    let mut m = MetaPage::new_for_capacity(8);
    m.alloc_page(); // page 3
    assert!(m.free_page(3));
    assert!(!m.free_page(3)); // already free
    assert_eq!(m.free_count, 5); // not double-decremented
}

#[test]
fn meta_free_out_of_range_returns_false() {
    let mut m = MetaPage::new_for_capacity(8);
    assert!(!m.free_page(8)); // == total_pages
    assert!(!m.free_page(100));
    assert_eq!(m.free_count, 5); // unchanged
}

#[test]
fn meta_serialize_roundtrip_1024() {
    let mut m = MetaPage::new_for_capacity(16);
    m.alloc_page();
    m.alloc_page();
    m.generation = 42;
    let mut buf = vec![0u8; 1024];
    m.write_to(&mut buf);
    let rt = MetaPage::from_bytes(&buf).expect("roundtrip failed");
    assert_eq!(rt.total_pages, m.total_pages);
    assert_eq!(rt.generation, m.generation);
    assert_eq!(rt.free_count, m.free_count);
    assert_eq!(rt.bitmap, m.bitmap);
}

#[test]
fn meta_serialize_roundtrip_4096() {
    let m = MetaPage::new_for_capacity(32);
    let mut buf = vec![0u8; 4096];
    m.write_to(&mut buf);
    let rt = MetaPage::from_bytes(&buf).unwrap();
    assert_eq!(rt.total_pages, 32);
    assert_eq!(rt.free_count, 29);
}

#[test]
fn meta_bad_checksum_rejected() {
    let m = MetaPage::new_for_capacity(8);
    let mut buf = vec![0u8; 1024];
    m.write_to(&mut buf);
    *buf.last_mut().unwrap() ^= 0xFF; // flip last checksum byte
    assert!(MetaPage::from_bytes(&buf).is_none());
}

#[test]
fn meta_corrupt_body_rejected() {
    let m = MetaPage::new_for_capacity(8);
    let mut buf = vec![0u8; 1024];
    m.write_to(&mut buf);
    buf[0] = 0xFF; // corrupt total_pages without updating checksum
    assert!(MetaPage::from_bytes(&buf).is_none());
}

#[test]
fn meta_too_short_rejected() {
    assert!(MetaPage::from_bytes(&[0u8; 27]).is_none());
}

#[test]
fn meta_grow_to_extends_capacity_and_freecount() {
    let mut m = MetaPage::new_for_capacity(4);
    m.alloc_page(); // page 3; free_count = 0
    m.grow_to(16);
    assert_eq!(m.total_pages, 16);
    assert_eq!(m.free_count, 12); // added 12 new pages
    assert_eq!(m.bitmap.len(), 2);
    assert_eq!(m.bitmap[1], 0x00); // pages 8-15 all free
}

#[test]
fn meta_grow_to_preserves_existing_allocations() {
    let mut m = MetaPage::new_for_capacity(4);
    m.alloc_page(); // page 3 allocated → bitmap[0] bit 3 = 1
    m.grow_to(16);
    assert_ne!(m.bitmap[0] & (1 << 3), 0); // page 3 still allocated
    // reserved pages still allocated
    for i in 0..FIRST_DATA_PAGE {
        assert_ne!(m.bitmap[0] & (1 << i), 0);
    }
}

#[test]
fn meta_grow_to_new_pages_are_free_and_allocatable() {
    let mut m = MetaPage::new_for_capacity(4);
    m.alloc_page(); // exhaust capacity
    m.grow_to(8);
    assert_eq!(m.alloc_page(), Some(4)); // first new page
}

// ── Superblock unit tests ─────────────────────────────────────────────────────

#[test]
fn superblock_roundtrip() {
    let sb = Superblock {
        magic: MAGIC,
        page_size_log2: 10,
        active_meta: MetaSelector::B,
        meta_checksum: 0xDEAD_BEEF,
    };
    let mut buf = [0u8; 20];
    sb.write_to(&mut buf);
    let rt = Superblock::from_bytes(&buf).unwrap();
    assert_eq!(rt.magic, sb.magic);
    assert_eq!(rt.page_size_log2, sb.page_size_log2);
    assert_eq!(rt.active_meta, sb.active_meta);
    assert_eq!(rt.meta_checksum, sb.meta_checksum);
}

#[test]
fn superblock_is_valid_correct_magic() {
    let sb = Superblock {
        magic: MAGIC,
        page_size_log2: 10,
        active_meta: MetaSelector::A,
        meta_checksum: 0,
    };
    assert!(sb.is_valid());
}

#[test]
fn superblock_wrong_magic_tag_invalid() {
    let bad = u64::from_le_bytes(*b"BADMAGIC");
    let sb = Superblock {
        magic: bad,
        page_size_log2: 10,
        active_meta: MetaSelector::A,
        meta_checksum: 0,
    };
    assert!(!sb.is_valid());
}

#[test]
fn superblock_wrong_major_invalid() {
    let mut b = MAGIC.to_le_bytes();
    b[4] = 99;
    let sb = Superblock {
        magic: u64::from_le_bytes(b),
        page_size_log2: 10,
        active_meta: MetaSelector::A,
        meta_checksum: 0,
    };
    assert!(!sb.is_valid());
}

#[test]
fn superblock_wrong_minor_invalid() {
    let mut b = MAGIC.to_le_bytes();
    b[5] = 99;
    let sb = Superblock {
        magic: u64::from_le_bytes(b),
        page_size_log2: 10,
        active_meta: MetaSelector::A,
        meta_checksum: 0,
    };
    assert!(!sb.is_valid());
}

#[test]
fn superblock_different_patch_and_build_still_valid() {
    let mut b = MAGIC.to_le_bytes();
    b[6] = 7; // different patch
    b[7] = 99; // different build
    let sb = Superblock {
        magic: u64::from_le_bytes(b),
        page_size_log2: 10,
        active_meta: MetaSelector::A,
        meta_checksum: 0,
    };
    assert!(sb.is_valid()); // patch/build not checked for compatibility
}

#[test]
fn superblock_too_short_returns_none() {
    assert!(Superblock::from_bytes(&[0u8; 19]).is_none());
}

// ── Pager integration tests ───────────────────────────────────────────────────

#[test]
fn pager_create_basic() {
    let tmp = TempPath::new();
    let p = Pager::create(tmp.path(), 10).unwrap();
    assert_eq!(p.page_size(), 1024);
    assert_eq!(p.page_count(), 4);
    assert_eq!(p.free_page_count(), 1);
}

#[test]
fn pager_create_larger_page_size() {
    let tmp = TempPath::new();
    let p = Pager::create(tmp.path(), 12).unwrap(); // 4096 bytes
    assert_eq!(p.page_size(), 4096);
}

#[test]
fn pager_create_page_size_too_small() {
    let tmp = TempPath::new();
    for log2 in [0u32, 5, 9] {
        assert!(matches!(
            Pager::create(tmp.path(), log2),
            Err(MappedPageError::InvalidPageSize)
        ));
    }
}

#[test]
fn pager_create_existing_file_fails() {
    let tmp = TempPath::new();
    Pager::create(tmp.path(), 10).unwrap();
    assert!(matches!(
        Pager::create(tmp.path(), 10),
        Err(MappedPageError::Io(_))
    ));
}

#[test]
fn pager_open_nonexistent_fails() {
    let tmp = TempPath::new();
    assert!(matches!(
        Pager::open(tmp.path()),
        Err(MappedPageError::Io(_))
    ));
}

#[test]
fn pager_open_recovers_state() {
    let tmp = TempPath::new();
    Pager::create(tmp.path(), 10).unwrap();
    let p = Pager::open(tmp.path()).unwrap();
    assert_eq!(p.page_size(), 1024);
    assert_eq!(p.page_count(), 4);
    assert_eq!(p.free_page_count(), 1);
}

#[test]
fn pager_alloc_returns_data_page() {
    let tmp = TempPath::new();
    let mut p = Pager::create(tmp.path(), 10).unwrap();
    let id = p.alloc().unwrap();
    assert!(
        id.0 >= FIRST_DATA_PAGE,
        "alloc must not return reserved pages"
    );
}

#[test]
fn pager_alloc_multiple_distinct_ids() {
    let tmp = TempPath::new();
    let mut p = Pager::create(tmp.path(), 10).unwrap();
    // 1 initial free page, then grows to accommodate more
    let ids: Vec<PageId> = (0..5).map(|_| p.alloc().unwrap()).collect();
    let mut sorted: Vec<u64> = ids.iter().map(|id| id.0).collect();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), 5, "all returned PageIds must be distinct");
    assert!(ids.iter().all(|id| id.0 >= FIRST_DATA_PAGE));
}

#[test]
fn pager_alloc_exhaustion_triggers_grow() {
    let tmp = TempPath::new();
    let mut p = Pager::create(tmp.path(), 10).unwrap();
    assert_eq!(p.page_count(), 4);
    p.alloc().unwrap(); // last free page (3)
    assert_eq!(p.free_page_count(), 0);
    p.alloc().unwrap(); // triggers grow 4 → 8, then allocates page 4
    assert_eq!(p.page_count(), 8);
    assert_eq!(p.free_page_count(), 3); // pages 5-7 still free
}

#[test]
fn pager_page_count_doubles_each_grow() {
    let tmp = TempPath::new();
    let mut p = Pager::create(tmp.path(), 10).unwrap();
    // exhaust and grow twice
    while p.free_page_count() > 0 {
        p.alloc().unwrap();
    }
    p.alloc().unwrap(); // grow 4 → 8
    assert_eq!(p.page_count(), 8);
    while p.free_page_count() > 0 {
        p.alloc().unwrap();
    }
    p.alloc().unwrap(); // grow 8 → 16
    assert_eq!(p.page_count(), 16);
}

#[test]
fn pager_free_returns_page_to_pool() {
    let tmp = TempPath::new();
    let mut p = Pager::create(tmp.path(), 10).unwrap();
    let id = p.alloc().unwrap();
    assert_eq!(p.free_page_count(), 0);
    p.free(id).unwrap();
    assert_eq!(p.free_page_count(), 1);
    let id2 = p.alloc().unwrap(); // should reuse
    assert_eq!(id, id2);
}

#[test]
fn pager_free_reserved_pages_error() {
    let tmp = TempPath::new();
    let mut p = Pager::create(tmp.path(), 10).unwrap();
    for i in 0..FIRST_DATA_PAGE {
        assert!(matches!(
            p.free(PageId(i)),
            Err(MappedPageError::ReservedPage)
        ));
    }
}

#[test]
fn pager_free_out_of_bounds_error() {
    let tmp = TempPath::new();
    let mut p = Pager::create(tmp.path(), 10).unwrap();
    assert!(matches!(
        p.free(PageId(4)),
        Err(MappedPageError::OutOfBounds)
    ));
    assert!(matches!(
        p.free(PageId(u64::MAX)),
        Err(MappedPageError::OutOfBounds)
    ));
}

#[test]
fn pager_double_free_error() {
    let tmp = TempPath::new();
    let mut p = Pager::create(tmp.path(), 10).unwrap();
    let id = p.alloc().unwrap();
    p.free(id).unwrap();
    assert!(matches!(p.free(id), Err(MappedPageError::DoubleFree)));
    assert_eq!(p.free_page_count(), 1); // count not corrupted
}

#[test]
fn pager_get_page_reserved_error() {
    let tmp = TempPath::new();
    let p = Pager::create(tmp.path(), 10).unwrap();
    for i in 0..FIRST_DATA_PAGE {
        assert!(matches!(
            PageId(i).get(&p),
            Err(MappedPageError::ReservedPage)
        ));
    }
}

#[test]
fn pager_get_page_out_of_bounds_error() {
    let tmp = TempPath::new();
    let p = Pager::create(tmp.path(), 10).unwrap();
    assert!(matches!(
        PageId(4).get(&p),
        Err(MappedPageError::OutOfBounds)
    ));
    assert!(matches!(
        PageId(u64::MAX).get(&p),
        Err(MappedPageError::OutOfBounds)
    ));
}

#[test]
fn pager_get_page_mut_write_then_read() {
    let tmp = TempPath::new();
    let mut p = Pager::create(tmp.path(), 10).unwrap();
    let id = p.alloc().unwrap();
    id.get_mut(&mut p).unwrap().as_bytes_mut().fill(0xAB);
    assert!(id.get(&p).unwrap().as_bytes().iter().all(|&b| b == 0xAB));
}

#[test]
fn pager_page_len_equals_page_size() {
    let tmp = TempPath::new();
    let mut p = Pager::create(tmp.path(), 10).unwrap();
    let id = p.alloc().unwrap();
    assert_eq!(id.get(&p).unwrap().len(), 1024);
}

#[test]
fn pager_data_persists_across_reopen() {
    let tmp = TempPath::new();
    let id = {
        let mut p = Pager::create(tmp.path(), 10).unwrap();
        let id = p.alloc().unwrap();
        id.get_mut(&mut p).unwrap().as_bytes_mut().fill(0x5A);
        id
    };
    let p = Pager::open(tmp.path()).unwrap();
    assert!(id.get(&p).unwrap().as_bytes().iter().all(|&b| b == 0x5A));
}

#[test]
fn pager_grow_preserves_existing_page_data() {
    let tmp = TempPath::new();
    let mut p = Pager::create(tmp.path(), 10).unwrap();
    let id = p.alloc().unwrap(); // page 3; exhausts capacity
    id.get_mut(&mut p).unwrap().as_bytes_mut().fill(0xCC);
    p.alloc().unwrap(); // triggers grow and remap
    assert!(id.get(&p).unwrap().as_bytes().iter().all(|&b| b == 0xCC));
}

#[test]
fn pager_alloc_free_sequence_correct_after_reopen() {
    let tmp = TempPath::new();
    let kept = {
        let mut p = Pager::create(tmp.path(), 10).unwrap();
        let a = p.alloc().unwrap(); // page 3
        let _b = p.alloc().unwrap(); // grows; page 4
        let c = p.alloc().unwrap(); // page 5
        p.free(a).unwrap();
        let d = p.alloc().unwrap(); // reuses page 3
        assert_eq!(d, a);
        p.free(c).unwrap();
        // allocated: page 3 (d), page 4 (_b)
        // free: page 3's old slot is now d, page 5 is free
        d
    };
    let p = Pager::open(tmp.path()).unwrap();
    assert_eq!(p.page_count(), 8);
    assert!(kept.get(&p).is_ok());
}

// ── Crash simulation tests ────────────────────────────────────────────────────
//
// These tests manually corrupt specific bytes in the file to simulate partial
// writes at each step of the commit protocol, then verify recovery.

#[test]
fn crash_corrupt_active_meta_falls_back_to_alternate() {
    let tmp = TempPath::new();
    {
        let mut p = Pager::create(tmp.path(), 10).unwrap();
        // commit: B gets new state (page 3 allocated), superblock → B
        p.alloc().unwrap();
    }
    // Active = B.  Corrupt B's embedded checksum → B fails on next open.
    zero_range(tmp.path(), meta_checksum_offset(MetaSelector::B, 1024), 4);

    let mut p = Pager::open(tmp.path()).unwrap();
    // Recovery must use A, which has the pre-alloc state.
    assert_eq!(p.active_meta_selector(), MetaSelector::A);
    assert_eq!(p.free_page_count(), 1); // page 3 free again
    assert_eq!(p.alloc().unwrap().0, 3);
}

#[test]
fn crash_corrupt_inactive_meta_has_no_effect() {
    let tmp = TempPath::new();
    {
        let mut p = Pager::create(tmp.path(), 10).unwrap();
        p.alloc().unwrap(); // active → B
    }
    // Corrupt the inactive page (A) — this should be irrelevant.
    zero_range(tmp.path(), meta_checksum_offset(MetaSelector::A, 1024), 4);

    let p = Pager::open(tmp.path()).unwrap();
    assert_eq!(p.active_meta_selector(), MetaSelector::B);
    assert_eq!(p.free_page_count(), 0); // page 3 still allocated
}

#[test]
fn crash_both_meta_corrupt_returns_error() {
    let tmp = TempPath::new();
    Pager::create(tmp.path(), 10).unwrap();
    zero_range(tmp.path(), meta_checksum_offset(MetaSelector::A, 1024), 4);
    zero_range(tmp.path(), meta_checksum_offset(MetaSelector::B, 1024), 4);
    assert!(matches!(
        Pager::open(tmp.path()),
        Err(MappedPageError::CorruptMetadata)
    ));
}

#[test]
fn crash_corrupt_superblock_magic() {
    let tmp = TempPath::new();
    Pager::create(tmp.path(), 10).unwrap();
    zero_range(tmp.path(), 0, 8); // zero the magic bytes
    assert!(matches!(
        Pager::open(tmp.path()),
        Err(MappedPageError::CorruptSuperblock)
    ));
}

#[test]
fn crash_corrupt_superblock_version_major() {
    let tmp = TempPath::new();
    Pager::create(tmp.path(), 10).unwrap();
    write_at(tmp.path(), 4, &[0xFF]); // byte 4 = version major
    assert!(matches!(
        Pager::open(tmp.path()),
        Err(MappedPageError::CorruptSuperblock)
    ));
}

#[test]
fn crash_corrupt_superblock_selector_byte() {
    let tmp = TempPath::new();
    Pager::create(tmp.path(), 10).unwrap();
    write_at(tmp.path(), 12, &[0xFF]); // byte 12 = active_meta; 0xFF is invalid
    assert!(matches!(
        Pager::open(tmp.path()),
        Err(MappedPageError::CorruptSuperblock)
    ));
}

#[test]
fn crash_superblock_meta_checksum_mismatch_falls_back() {
    let tmp = TempPath::new();
    {
        let mut p = Pager::create(tmp.path(), 10).unwrap();
        p.alloc().unwrap(); // active → B; page 3 allocated
    }
    // Zero the superblock's meta_checksum field (bytes 16-19).
    // Superblock still points to B but checksum no longer matches.
    zero_range(tmp.path(), 16, 4);

    // B fails the superblock-checksum cross-check; falls back to A.
    let mut p = Pager::open(tmp.path()).unwrap();
    assert_eq!(p.active_meta_selector(), MetaSelector::A);
    assert_eq!(p.free_page_count(), 1);
    assert_eq!(p.alloc().unwrap().0, 3);
}

/// Simulate a crash between commit step 1 (write inactive meta) and step 2
/// (flip superblock).  The newer state written to B is discarded; recovery
/// uses A (the still-active page per the superblock).
#[test]
fn crash_between_meta_write_and_superblock_flip() {
    let tmp = TempPath::new();
    // Create: active = A, page 3 free.
    Pager::create(tmp.path(), 10).unwrap();

    // Manually write a "newer" committed-looking MetaPage to B (page 3
    // allocated, generation bumped) — as if commit step 1 completed then
    // the process died before step 2.
    {
        let mut new_meta = MetaPage::new_for_capacity(4);
        new_meta.alloc_page(); // mark page 3 allocated
        new_meta.generation = 1;
        let mut buf = vec![0u8; 1024];
        new_meta.write_to(&mut buf);
        write_at(tmp.path(), MetaSelector::B.page_id() * 1024, &buf);
    }
    // Superblock still says active = A with original checksum.

    let mut p = Pager::open(tmp.path()).unwrap();
    // Must use A: the uncommitted B is ignored.
    assert_eq!(p.active_meta_selector(), MetaSelector::A);
    assert_eq!(p.free_page_count(), 1);
    assert_eq!(p.alloc().unwrap().0, 3);
}

#[test]
fn crash_committed_alloc_survives_reopen() {
    let tmp = TempPath::new();
    let id = {
        let mut p = Pager::create(tmp.path(), 10).unwrap();
        p.alloc().unwrap()
    };
    // Reopen: page must still be allocated.
    let mut p = Pager::open(tmp.path()).unwrap();
    assert_eq!(p.free_page_count(), 0);
    // Freeing it must succeed (proves it was allocated, not silently lost).
    p.free(id).unwrap();
    assert!(matches!(p.free(id), Err(MappedPageError::DoubleFree)));
}

#[test]
fn crash_committed_data_survives_reopen() {
    const PAT: u8 = 0xA5;
    let tmp = TempPath::new();
    let id = {
        let mut p = Pager::create(tmp.path(), 10).unwrap();
        let id = p.alloc().unwrap();
        id.get_mut(&mut p).unwrap().as_bytes_mut().fill(PAT);
        id
    };
    let p = Pager::open(tmp.path()).unwrap();
    assert!(id.get(&p).unwrap().as_bytes().iter().all(|&b| b == PAT));
}

#[test]
fn crash_grow_survives_reopen() {
    let tmp = TempPath::new();
    {
        let mut p = Pager::create(tmp.path(), 10).unwrap();
        p.alloc().unwrap(); // exhaust
        p.alloc().unwrap(); // grow 4 → 8
    }
    let p = Pager::open(tmp.path()).unwrap();
    assert_eq!(p.page_count(), 8);
    // first alloc took page 3; grow added pages 4-7; second alloc took page 4 → 3 free
    assert_eq!(p.free_page_count(), 3);
}

#[test]
fn crash_multiple_grows_survive_reopen() {
    let tmp = TempPath::new();
    let alloc_count;
    {
        let mut p = Pager::create(tmp.path(), 10).unwrap();
        // grow twice: 4 → 8 → 16
        let mut n = 0u32;
        while p.page_count() < 16 {
            p.alloc().unwrap();
            n += 1;
        }
        alloc_count = n;
        // write a marker to the last allocated page
        let last_id = PageId(FIRST_DATA_PAGE + alloc_count as u64 - 1);
        last_id.get_mut(&mut p).unwrap().as_bytes_mut()[0] = 0xBB;
    }
    let p = Pager::open(tmp.path()).unwrap();
    assert_eq!(p.page_count(), 16);
    let last_id = PageId(FIRST_DATA_PAGE + alloc_count as u64 - 1);
    assert_eq!(last_id.get(&p).unwrap().as_bytes()[0], 0xBB);
}
