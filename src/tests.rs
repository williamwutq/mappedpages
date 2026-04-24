use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use crate::concurrent::{ConcurrentPager, ConcurrentPagerError};
use crate::meta::{
    DirBlockRef, DirEntry, DirPage, FIRST_DATA_PAGE, MAGIC, MetaPage, MetaSelector, Superblock,
    dir_entries_per_page, read_dir_blocks, write_dir_blocks,
};
use crate::{MappedPageError, PageId, Pager, ProtectedPageId};

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
    let p = Pager::<1024>::create(tmp.path()).unwrap();
    assert_eq!(p.page_size(), 1024);
    assert_eq!(p.page_count(), 4);
    assert_eq!(p.free_page_count(), 1);
}

#[test]
fn pager_create_larger_page_size() {
    let tmp = TempPath::new();
    let p = Pager::<4096>::create(tmp.path()).unwrap();
    assert_eq!(p.page_size(), 4096);
}

#[test]
fn pager_create_existing_file_fails() {
    let tmp = TempPath::new();
    Pager::<1024>::create(tmp.path()).unwrap();
    assert!(matches!(
        Pager::<1024>::create(tmp.path()),
        Err(MappedPageError::Io(_))
    ));
}

#[test]
fn pager_open_nonexistent_fails() {
    let tmp = TempPath::new();
    assert!(matches!(
        Pager::<1024>::open(tmp.path()),
        Err(MappedPageError::Io(_))
    ));
}

#[test]
fn pager_open_recovers_state() {
    let tmp = TempPath::new();
    Pager::<1024>::create(tmp.path()).unwrap();
    let p = Pager::<1024>::open(tmp.path()).unwrap();
    assert_eq!(p.page_size(), 1024);
    assert_eq!(p.page_count(), 4);
    assert_eq!(p.free_page_count(), 1);
}

#[test]
fn pager_open_wrong_page_size_fails() {
    let tmp = TempPath::new();
    Pager::<1024>::create(tmp.path()).unwrap();
    assert!(matches!(
        Pager::<4096>::open(tmp.path()),
        Err(MappedPageError::InvalidPageSize)
    ));
}

#[test]
fn pager_alloc_returns_data_page() {
    let tmp = TempPath::new();
    let mut p = Pager::<1024>::create(tmp.path()).unwrap();
    let id = p.alloc().unwrap();
    assert!(
        id.0 >= FIRST_DATA_PAGE,
        "alloc must not return reserved pages"
    );
}

#[test]
fn pager_alloc_multiple_distinct_ids() {
    let tmp = TempPath::new();
    let mut p = Pager::<1024>::create(tmp.path()).unwrap();
    // 1 initial free page, then grows to accommodate more
    let ids: Vec<PageId<1024>> = (0..5).map(|_| p.alloc().unwrap()).collect();
    let mut sorted: Vec<u64> = ids.iter().map(|id| id.0).collect();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), 5, "all returned PageIds must be distinct");
    assert!(ids.iter().all(|id| id.0 >= FIRST_DATA_PAGE));
}

#[test]
fn pager_alloc_exhaustion_triggers_grow() {
    let tmp = TempPath::new();
    let mut p = Pager::<1024>::create(tmp.path()).unwrap();
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
    let mut p = Pager::<1024>::create(tmp.path()).unwrap();
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
    let mut p = Pager::<1024>::create(tmp.path()).unwrap();
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
    let mut p = Pager::<1024>::create(tmp.path()).unwrap();
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
    let mut p = Pager::<1024>::create(tmp.path()).unwrap();
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
    let mut p = Pager::<1024>::create(tmp.path()).unwrap();
    let id = p.alloc().unwrap();
    p.free(id).unwrap();
    assert!(matches!(p.free(id), Err(MappedPageError::DoubleFree)));
    assert_eq!(p.free_page_count(), 1); // count not corrupted
}

#[test]
fn pager_get_page_reserved_error() {
    let tmp = TempPath::new();
    let p = Pager::<1024>::create(tmp.path()).unwrap();
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
    let p = Pager::<1024>::create(tmp.path()).unwrap();
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
    let mut p = Pager::<1024>::create(tmp.path()).unwrap();
    let id = p.alloc().unwrap();
    id.get_mut(&mut p).unwrap().as_bytes_mut().fill(0xAB);
    assert!(id.get(&p).unwrap().as_bytes().iter().all(|&b| b == 0xAB));
}

#[test]
fn pager_page_len_equals_page_size() {
    let tmp = TempPath::new();
    let mut p = Pager::<1024>::create(tmp.path()).unwrap();
    let id = p.alloc().unwrap();
    assert_eq!(id.get(&p).unwrap().len(), 1024);
}

#[test]
fn pager_data_persists_across_reopen() {
    let tmp = TempPath::new();
    let id = {
        let mut p = Pager::<1024>::create(tmp.path()).unwrap();
        let id = p.alloc().unwrap();
        id.get_mut(&mut p).unwrap().as_bytes_mut().fill(0x5A);
        id
    };
    let p = Pager::<1024>::open(tmp.path()).unwrap();
    assert!(id.get(&p).unwrap().as_bytes().iter().all(|&b| b == 0x5A));
}

#[test]
fn pager_grow_preserves_existing_page_data() {
    let tmp = TempPath::new();
    let mut p = Pager::<1024>::create(tmp.path()).unwrap();
    let id = p.alloc().unwrap(); // page 3; exhausts capacity
    id.get_mut(&mut p).unwrap().as_bytes_mut().fill(0xCC);
    p.alloc().unwrap(); // triggers grow and remap
    assert!(id.get(&p).unwrap().as_bytes().iter().all(|&b| b == 0xCC));
}

#[test]
fn pager_alloc_free_sequence_correct_after_reopen() {
    let tmp = TempPath::new();
    let kept = {
        let mut p = Pager::<1024>::create(tmp.path()).unwrap();
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
    let p = Pager::<1024>::open(tmp.path()).unwrap();
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
        let mut p = Pager::<1024>::create(tmp.path()).unwrap();
        // commit: B gets new state (page 3 allocated), superblock → B
        p.alloc().unwrap();
    }
    // Active = B.  Corrupt B's embedded checksum → B fails on next open.
    zero_range(tmp.path(), meta_checksum_offset(MetaSelector::B, 1024), 4);

    let mut p = Pager::<1024>::open(tmp.path()).unwrap();
    // Recovery must use A, which has the pre-alloc state.
    assert_eq!(p.active_meta_selector(), MetaSelector::A);
    assert_eq!(p.free_page_count(), 1); // page 3 free again
    assert_eq!(p.alloc().unwrap().0, 3);
}

#[test]
fn crash_corrupt_inactive_meta_has_no_effect() {
    let tmp = TempPath::new();
    {
        let mut p = Pager::<1024>::create(tmp.path()).unwrap();
        p.alloc().unwrap(); // active → B
    }
    // Corrupt the inactive page (A) — this should be irrelevant.
    zero_range(tmp.path(), meta_checksum_offset(MetaSelector::A, 1024), 4);

    let p = Pager::<1024>::open(tmp.path()).unwrap();
    assert_eq!(p.active_meta_selector(), MetaSelector::B);
    assert_eq!(p.free_page_count(), 0); // page 3 still allocated
}

#[test]
fn crash_both_meta_corrupt_returns_error() {
    let tmp = TempPath::new();
    Pager::<1024>::create(tmp.path()).unwrap();
    zero_range(tmp.path(), meta_checksum_offset(MetaSelector::A, 1024), 4);
    zero_range(tmp.path(), meta_checksum_offset(MetaSelector::B, 1024), 4);
    assert!(matches!(
        Pager::<1024>::open(tmp.path()),
        Err(MappedPageError::CorruptMetadata)
    ));
}

#[test]
fn crash_corrupt_superblock_magic() {
    let tmp = TempPath::new();
    Pager::<1024>::create(tmp.path()).unwrap();
    zero_range(tmp.path(), 0, 8); // zero the magic bytes
    assert!(matches!(
        Pager::<1024>::open(tmp.path()),
        Err(MappedPageError::CorruptSuperblock)
    ));
}

#[test]
fn crash_corrupt_superblock_version_major() {
    let tmp = TempPath::new();
    Pager::<1024>::create(tmp.path()).unwrap();
    write_at(tmp.path(), 4, &[0xFF]); // byte 4 = version major
    assert!(matches!(
        Pager::<1024>::open(tmp.path()),
        Err(MappedPageError::CorruptSuperblock)
    ));
}

#[test]
fn crash_corrupt_superblock_selector_byte() {
    let tmp = TempPath::new();
    Pager::<1024>::create(tmp.path()).unwrap();
    write_at(tmp.path(), 12, &[0xFF]); // byte 12 = active_meta; 0xFF is invalid
    assert!(matches!(
        Pager::<1024>::open(tmp.path()),
        Err(MappedPageError::CorruptSuperblock)
    ));
}

#[test]
fn crash_superblock_meta_checksum_mismatch_falls_back() {
    let tmp = TempPath::new();
    {
        let mut p = Pager::<1024>::create(tmp.path()).unwrap();
        p.alloc().unwrap(); // active → B; page 3 allocated
    }
    // Zero the superblock's meta_checksum field (bytes 16-19).
    // Superblock still points to B but checksum no longer matches.
    zero_range(tmp.path(), 16, 4);

    // B fails the superblock-checksum cross-check; falls back to A.
    let mut p = Pager::<1024>::open(tmp.path()).unwrap();
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
    Pager::<1024>::create(tmp.path()).unwrap();

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

    let mut p = Pager::<1024>::open(tmp.path()).unwrap();
    // Must use A: the uncommitted B is ignored.
    assert_eq!(p.active_meta_selector(), MetaSelector::A);
    assert_eq!(p.free_page_count(), 1);
    assert_eq!(p.alloc().unwrap().0, 3);
}

#[test]
fn crash_committed_alloc_survives_reopen() {
    let tmp = TempPath::new();
    let id = {
        let mut p = Pager::<1024>::create(tmp.path()).unwrap();
        p.alloc().unwrap()
    };
    // Reopen: page must still be allocated.
    let mut p = Pager::<1024>::open(tmp.path()).unwrap();
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
        let mut p = Pager::<1024>::create(tmp.path()).unwrap();
        let id = p.alloc().unwrap();
        id.get_mut(&mut p).unwrap().as_bytes_mut().fill(PAT);
        id
    };
    let p = Pager::<1024>::open(tmp.path()).unwrap();
    assert!(id.get(&p).unwrap().as_bytes().iter().all(|&b| b == PAT));
}

#[test]
fn crash_grow_survives_reopen() {
    let tmp = TempPath::new();
    {
        let mut p = Pager::<1024>::create(tmp.path()).unwrap();
        p.alloc().unwrap(); // exhaust
        p.alloc().unwrap(); // grow 4 → 8
    }
    let p = Pager::<1024>::open(tmp.path()).unwrap();
    assert_eq!(p.page_count(), 8);
    // first alloc took page 3; grow added pages 4-7; second alloc took page 4 → 3 free
    assert_eq!(p.free_page_count(), 3);
}

#[test]
fn crash_multiple_grows_survive_reopen() {
    let tmp = TempPath::new();
    let alloc_count;
    {
        let mut p = Pager::<1024>::create(tmp.path()).unwrap();
        // grow twice: 4 → 8 → 16
        let mut n = 0u32;
        while p.page_count() < 16 {
            p.alloc().unwrap();
            n += 1;
        }
        alloc_count = n;
        // write a marker to the last allocated page
        let last_id = PageId::<1024>(FIRST_DATA_PAGE + alloc_count as u64 - 1);
        last_id.get_mut(&mut p).unwrap().as_bytes_mut()[0] = 0xBB;
    }
    let p = Pager::<1024>::open(tmp.path()).unwrap();
    assert_eq!(p.page_count(), 16);
    let last_id = PageId::<1024>(FIRST_DATA_PAGE + alloc_count as u64 - 1);
    assert_eq!(last_id.get(&p).unwrap().as_bytes()[0], 0xBB);
}

// ── DirPage unit tests ────────────────────────────────────────────────────────

#[test]
fn dir_page_new_empty_has_correct_capacity() {
    let dp = DirPage::new_empty(1024);
    let expected = dir_entries_per_page(1024) as u32;
    assert_eq!(dp.capacity, expected);
    assert_eq!(dp.entries.len(), expected as usize);
    assert!(dp.entries.iter().all(|e| !e.in_use));
}

#[test]
fn dir_page_roundtrip_empty() {
    let dp = DirPage::new_empty(1024);
    let mut buf = vec![0u8; 1024];
    dp.write_to(&mut buf);
    let rt = DirPage::from_bytes(&buf).expect("roundtrip failed");
    assert_eq!(rt.capacity, dp.capacity);
    assert_eq!(rt.entries.len(), dp.entries.len());
    assert!(rt.entries.iter().all(|e| !e.in_use));
}

#[test]
fn dir_page_roundtrip_with_entries() {
    let mut dp = DirPage::new_empty(1024);
    dp.entries[0] = DirEntry {
        in_use: true,
        page_a: 5,
        page_b: 6,
        active_slot: 1,
        generation: 42,
        checksum: 0xDEAD_BEEF,
    };
    dp.entries[2] = DirEntry {
        in_use: true,
        page_a: 100,
        page_b: 101,
        active_slot: 0,
        generation: 7,
        checksum: 0x1234_5678,
    };
    let mut buf = vec![0u8; 1024];
    dp.write_to(&mut buf);
    let rt = DirPage::from_bytes(&buf).unwrap();
    assert!(rt.entries[0].in_use);
    assert_eq!(rt.entries[0].page_a, 5);
    assert_eq!(rt.entries[0].page_b, 6);
    assert_eq!(rt.entries[0].active_slot, 1);
    assert_eq!(rt.entries[0].generation, 42);
    assert_eq!(rt.entries[0].checksum, 0xDEAD_BEEF);
    assert!(!rt.entries[1].in_use);
    assert!(rt.entries[2].in_use);
    assert_eq!(rt.entries[2].page_a, 100);
}

#[test]
fn dir_page_bad_checksum_rejected() {
    let dp = DirPage::new_empty(1024);
    let mut buf = vec![0u8; 1024];
    dp.write_to(&mut buf);
    *buf.last_mut().unwrap() ^= 0xFF;
    assert!(DirPage::from_bytes(&buf).is_none());
}

#[test]
fn dir_page_too_short_rejected() {
    assert!(DirPage::from_bytes(&[0u8; 11]).is_none());
}

#[test]
fn dir_page_roundtrip_4096() {
    let dp = DirPage::new_empty(4096);
    let expected_cap = dir_entries_per_page(4096) as u32;
    assert!(expected_cap > dir_entries_per_page(1024) as u32);
    let mut buf = vec![0u8; 4096];
    dp.write_to(&mut buf);
    let rt = DirPage::from_bytes(&buf).unwrap();
    assert_eq!(rt.capacity, expected_cap);
}

// ── read_dir_blocks / write_dir_blocks unit tests ─────────────────────────────

#[test]
fn dir_blocks_empty_roundtrip() {
    let mut page0 = vec![0u8; 1024];
    write_dir_blocks(&[], &mut page0);
    let result = read_dir_blocks(&page0).unwrap();
    assert!(result.is_empty());
}

#[test]
fn dir_blocks_old_format_zeros_accepted() {
    // Old file: bytes 20..page_size are all zero → count == 0 → no dir blocks.
    let page0 = vec![0u8; 1024];
    let result = read_dir_blocks(&page0).unwrap();
    assert!(result.is_empty());
}

#[test]
fn dir_blocks_roundtrip_one_block() {
    let blocks = vec![DirBlockRef {
        page_a: 3,
        page_b: 4,
        active: MetaSelector::B,
    }];
    let mut page0 = vec![0u8; 1024];
    write_dir_blocks(&blocks, &mut page0);
    let rt = read_dir_blocks(&page0).unwrap();
    assert_eq!(rt.len(), 1);
    assert_eq!(rt[0].page_a, 3);
    assert_eq!(rt[0].page_b, 4);
    assert_eq!(rt[0].active, MetaSelector::B);
}

#[test]
fn dir_blocks_roundtrip_multiple_blocks() {
    let blocks = vec![
        DirBlockRef {
            page_a: 3,
            page_b: 4,
            active: MetaSelector::A,
        },
        DirBlockRef {
            page_a: 10,
            page_b: 11,
            active: MetaSelector::B,
        },
        DirBlockRef {
            page_a: 20,
            page_b: 21,
            active: MetaSelector::A,
        },
    ];
    let mut page0 = vec![0u8; 1024];
    write_dir_blocks(&blocks, &mut page0);
    let rt = read_dir_blocks(&page0).unwrap();
    assert_eq!(rt.len(), 3);
    assert_eq!(rt[1].page_a, 10);
    assert_eq!(rt[2].active, MetaSelector::A);
}

#[test]
fn dir_blocks_bad_checksum_rejected() {
    let blocks = vec![DirBlockRef {
        page_a: 3,
        page_b: 4,
        active: MetaSelector::A,
    }];
    let mut page0 = vec![0u8; 1024];
    write_dir_blocks(&blocks, &mut page0);
    let len = page0.len();
    page0[len - 1] ^= 0xFF; // corrupt dir section checksum
    assert!(read_dir_blocks(&page0).is_err());
}

// ── Protected-page integration tests ─────────────────────────────────────────

#[test]
fn protected_alloc_returns_id() {
    let tmp = TempPath::new();
    let mut p = Pager::<1024>::create(tmp.path()).unwrap();
    let id = p.alloc_protected().unwrap();
    assert_eq!(id.0, 0);
}

#[test]
fn protected_alloc_multiple_distinct_sequential_ids() {
    let tmp = TempPath::new();
    let mut p = Pager::<1024>::create(tmp.path()).unwrap();
    let ids: Vec<u64> = (0..4).map(|_| p.alloc_protected().unwrap().0).collect();
    assert_eq!(ids, [0, 1, 2, 3]);
}

#[test]
fn protected_page_len_equals_page_size() {
    let tmp = TempPath::new();
    let mut p = Pager::<1024>::create(tmp.path()).unwrap();
    let id = p.alloc_protected().unwrap();
    assert_eq!(id.get(&p).unwrap().len(), 1024);
}

#[test]
fn protected_read_initial_content_is_zero() {
    let tmp = TempPath::new();
    let mut p = Pager::<1024>::create(tmp.path()).unwrap();
    let id = p.alloc_protected().unwrap();
    assert!(id.get(&p).unwrap().as_bytes().iter().all(|&b| b == 0));
}

#[test]
fn protected_write_not_visible_before_commit() {
    let tmp = TempPath::new();
    let mut p = Pager::<1024>::create(tmp.path()).unwrap();
    let id = p.alloc_protected().unwrap();
    {
        let mut w = id.get_mut(&mut p).unwrap();
        w.page_mut().as_bytes_mut().fill(0xFF);
        // drop without commit
    }
    // Active physical page was never touched; must still read as zeros.
    assert!(id.get(&p).unwrap().as_bytes().iter().all(|&b| b == 0));
}

#[test]
fn protected_write_visible_after_commit() {
    let tmp = TempPath::new();
    let mut p = Pager::<1024>::create(tmp.path()).unwrap();
    let id = p.alloc_protected().unwrap();
    let mut w = id.get_mut(&mut p).unwrap();
    w.page_mut().as_bytes_mut().fill(0xAB);
    w.commit().unwrap();
    assert!(id.get(&p).unwrap().as_bytes().iter().all(|&b| b == 0xAB));
}

#[test]
fn protected_second_write_overwrites_first() {
    let tmp = TempPath::new();
    let mut p = Pager::<1024>::create(tmp.path()).unwrap();
    let id = p.alloc_protected().unwrap();

    let mut w = id.get_mut(&mut p).unwrap();
    w.page_mut().as_bytes_mut().fill(0xAB);
    w.commit().unwrap();

    let mut w2 = id.get_mut(&mut p).unwrap();
    w2.page_mut().as_bytes_mut().fill(0xCD);
    w2.commit().unwrap();

    assert!(id.get(&p).unwrap().as_bytes().iter().all(|&b| b == 0xCD));
}

#[test]
fn protected_alternating_writes_toggle_active_slot() {
    let tmp = TempPath::new();
    let mut p = Pager::<1024>::create(tmp.path()).unwrap();
    let id = p.alloc_protected().unwrap();

    // initial: active_slot = 0
    assert_eq!(p.protected_active_slot(id), 0);

    let mut w = id.get_mut(&mut p).unwrap();
    w.page_mut().as_bytes_mut().fill(1);
    w.commit().unwrap();
    assert_eq!(p.protected_active_slot(id), 1); // flipped

    let mut w2 = id.get_mut(&mut p).unwrap();
    w2.page_mut().as_bytes_mut().fill(2);
    w2.commit().unwrap();
    assert_eq!(p.protected_active_slot(id), 0); // flipped back

    let mut w3 = id.get_mut(&mut p).unwrap();
    w3.page_mut().as_bytes_mut().fill(3);
    w3.commit().unwrap();
    assert_eq!(p.protected_active_slot(id), 1);
}

#[test]
fn protected_free_releases_backing_pages() {
    let tmp = TempPath::new();
    let mut p = Pager::<1024>::create(tmp.path()).unwrap();
    p.alloc_protected().unwrap(); // first alloc: creates dir block + backing pages
    let id = p.alloc_protected().unwrap(); // second alloc: reuses dir block, new backing pages
    // Measure free count AFTER the second alloc (and any grow it triggered).
    let free_before = p.free_page_count();
    p.free_protected(id).unwrap(); // releases exactly 2 backing pages
    assert_eq!(p.free_page_count(), free_before + 2);
}

#[test]
fn protected_double_free_error() {
    let tmp = TempPath::new();
    let mut p = Pager::<1024>::create(tmp.path()).unwrap();
    let id = p.alloc_protected().unwrap();
    p.free_protected(id).unwrap();
    assert!(matches!(
        p.free_protected(id),
        Err(MappedPageError::DoubleFree)
    ));
}

#[test]
fn protected_get_invalid_id_returns_out_of_bounds() {
    let tmp = TempPath::new();
    let p = Pager::<1024>::create(tmp.path()).unwrap();
    // No dir blocks exist yet; any slot index is out of bounds.
    assert!(matches!(
        ProtectedPageId(0).get(&p),
        Err(MappedPageError::OutOfBounds)
    ));
    assert!(matches!(
        ProtectedPageId(u64::MAX).get(&p),
        Err(MappedPageError::OutOfBounds)
    ));
}

#[test]
fn protected_get_freed_slot_returns_out_of_bounds() {
    let tmp = TempPath::new();
    let mut p = Pager::<1024>::create(tmp.path()).unwrap();
    let id = p.alloc_protected().unwrap();
    p.free_protected(id).unwrap();
    assert!(matches!(id.get(&p), Err(MappedPageError::OutOfBounds)));
}

#[test]
fn protected_slot_reuse_after_free() {
    let tmp = TempPath::new();
    let mut p = Pager::<1024>::create(tmp.path()).unwrap();
    let id0 = p.alloc_protected().unwrap();

    // Write something, commit, then free.
    let mut w = id0.get_mut(&mut p).unwrap();
    w.page_mut().as_bytes_mut().fill(0xAB);
    w.commit().unwrap();
    p.free_protected(id0).unwrap();

    // Realloc must reuse slot 0.
    let id1 = p.alloc_protected().unwrap();
    assert_eq!(id1.0, 0);
    // New backing pages are freshly allocated → content is zero, not 0xAB.
    assert!(id1.get(&p).unwrap().as_bytes().iter().all(|&b| b == 0));
}

#[test]
fn protected_alloc_survives_reopen() {
    let tmp = TempPath::new();
    let id = {
        let mut p = Pager::<1024>::create(tmp.path()).unwrap();
        p.alloc_protected().unwrap()
    };
    let p = Pager::<1024>::open(tmp.path()).unwrap();
    // Slot must be accessible and marked in-use after reopen.
    assert!(id.get(&p).is_ok());
}

#[test]
fn protected_write_survives_reopen() {
    let tmp = TempPath::new();
    let id = {
        let mut p = Pager::<1024>::create(tmp.path()).unwrap();
        let id = p.alloc_protected().unwrap();
        let mut w = id.get_mut(&mut p).unwrap();
        w.page_mut().as_bytes_mut().fill(0x5C);
        w.commit().unwrap();
        id
    };
    let p = Pager::<1024>::open(tmp.path()).unwrap();
    assert!(id.get(&p).unwrap().as_bytes().iter().all(|&b| b == 0x5C));
}

#[test]
fn protected_multiple_writes_survive_reopen() {
    const PAT1: u8 = 0xAA;
    const PAT2: u8 = 0xBB;
    let tmp = TempPath::new();
    let (id0, id1) = {
        let mut p = Pager::<1024>::create(tmp.path()).unwrap();
        let id0 = p.alloc_protected().unwrap();
        let id1 = p.alloc_protected().unwrap();

        let mut w = id0.get_mut(&mut p).unwrap();
        w.page_mut().as_bytes_mut().fill(PAT1);
        w.commit().unwrap();

        let mut w = id1.get_mut(&mut p).unwrap();
        w.page_mut().as_bytes_mut().fill(PAT2);
        w.commit().unwrap();
        (id0, id1)
    };
    let p = Pager::<1024>::open(tmp.path()).unwrap();
    assert!(id0.get(&p).unwrap().as_bytes().iter().all(|&b| b == PAT1));
    assert!(id1.get(&p).unwrap().as_bytes().iter().all(|&b| b == PAT2));
}

#[test]
fn protected_free_survives_reopen() {
    let tmp = TempPath::new();
    let id = {
        let mut p = Pager::<1024>::create(tmp.path()).unwrap();
        let id = p.alloc_protected().unwrap();
        p.free_protected(id).unwrap();
        id
    };
    // Slot is free after reopen; getting it must fail.
    let p = Pager::<1024>::open(tmp.path()).unwrap();
    assert!(matches!(id.get(&p), Err(MappedPageError::OutOfBounds)));
}

#[test]
fn protected_normal_alloc_and_protected_alloc_coexist() {
    let tmp = TempPath::new();
    let mut p = Pager::<1024>::create(tmp.path()).unwrap();

    let normal_id = p.alloc().unwrap();
    let prot_id = p.alloc_protected().unwrap();
    let normal_id2 = p.alloc().unwrap();

    normal_id.get_mut(&mut p).unwrap().as_bytes_mut().fill(0x11);

    let mut w = prot_id.get_mut(&mut p).unwrap();
    w.page_mut().as_bytes_mut().fill(0x22);
    w.commit().unwrap();

    normal_id2
        .get_mut(&mut p)
        .unwrap()
        .as_bytes_mut()
        .fill(0x33);

    assert!(
        normal_id
            .get(&p)
            .unwrap()
            .as_bytes()
            .iter()
            .all(|&b| b == 0x11)
    );
    assert!(
        prot_id
            .get(&p)
            .unwrap()
            .as_bytes()
            .iter()
            .all(|&b| b == 0x22)
    );
    assert!(
        normal_id2
            .get(&p)
            .unwrap()
            .as_bytes()
            .iter()
            .all(|&b| b == 0x33)
    );
}

// ── Protected-page crash simulation tests ─────────────────────────────────────

/// Helper: byte offset of the last 4 bytes (checksum) of a physical page.
fn phys_page_checksum_offset(phys: u64, page_size: usize) -> u64 {
    phys * page_size as u64 + page_size as u64 - 4
}

/// Helper: byte offset of the start of a physical page.
fn phys_page_offset(phys: u64, page_size: usize) -> u64 {
    phys * page_size as u64
}

#[test]
fn crash_corrupt_active_dir_page_falls_back_to_inactive() {
    let tmp = TempPath::new();

    // Commit a write so the active dir page records active_slot=1.
    let id = {
        let mut p = Pager::<1024>::create(tmp.path()).unwrap();
        let id = p.alloc_protected().unwrap();
        let mut w = id.get_mut(&mut p).unwrap();
        w.page_mut().as_bytes_mut().fill(0xAB);
        w.commit().unwrap();
        // Active dir page is now A (page_a of the dir block).
        // Verify active_slot = 1 (page_b of the data is active).
        assert_eq!(p.protected_active_slot(id), 1);
        let (dir_pa, _dir_pb, _active) = p.dir_block_pages(0);
        // Active dir page is stored at dir_pa after the flip.
        zero_range(tmp.path(), phys_page_checksum_offset(dir_pa, 1024), 4);
        id
    };

    // Reopen: active dir page is corrupt → fall back to inactive (old state, slot=0).
    let p = Pager::<1024>::open(tmp.path()).unwrap();
    // Slot 0 of the fallback page has active_slot=0 (pre-write state).
    assert_eq!(p.protected_active_slot(id), 0);
    // The active physical page (page_a of data) was never written → zeros.
    assert!(id.get(&p).unwrap().as_bytes().iter().all(|&b| b == 0));
}

#[test]
fn crash_corrupt_both_dir_pages_returns_error() {
    let tmp = TempPath::new();
    {
        let mut p = Pager::<1024>::create(tmp.path()).unwrap();
        p.alloc_protected().unwrap();
        let (dir_pa, dir_pb, _) = p.dir_block_pages(0);
        zero_range(tmp.path(), phys_page_checksum_offset(dir_pa, 1024), 4);
        zero_range(tmp.path(), phys_page_checksum_offset(dir_pb, 1024), 4);
    }
    assert!(matches!(
        Pager::<1024>::open(tmp.path()),
        Err(MappedPageError::CorruptProtectedDirectory)
    ));
}

/// Simulate crash between writing the inactive dir page (step 1 of commit_dir_block)
/// and updating page 0 (step 3). Page 0 still points to the old active dir page, so
/// the committed write appears uncommitted after recovery.
#[test]
fn crash_between_dir_write_and_page0_update() {
    let tmp = TempPath::new();

    let id = {
        let mut p = Pager::<1024>::create(tmp.path()).unwrap();
        let id = p.alloc_protected().unwrap();
        // After alloc_protected: block.active = B (dir_pb).
        // Backing pages: data_pa is active (slot 0).
        let (dir_pa, dir_pb, active) = p.dir_block_pages(0);
        assert_eq!(active, MetaSelector::B); // active dir page is B=dir_pb
        let (_data_pa, data_pb) = p.protected_backing_pages(id);

        // Now simulate: writer fills data_pb (the inactive data page) with 0xAB
        // and the dir commit writes the new dir page to inactive dir_pa (A)...
        // but page 0 is NOT updated (crash before step 3).

        // Write 0xAB to the inactive data page.
        write_at(
            tmp.path(),
            phys_page_offset(data_pb, 1024),
            &vec![0xABu8; 1024],
        );

        // Write a valid dir page to dir_pa (currently inactive/A) reflecting
        // active_slot=1 (data_pb active) — as if commit_dir_block step 1 completed.
        let mut new_dir = DirPage::new_empty(1024);
        new_dir.entries[0] = DirEntry {
            in_use: true,
            page_a: p.protected_backing_pages(id).0,
            page_b: data_pb,
            active_slot: 1,
            generation: 1,
            checksum: crc32fast::hash(&vec![0xABu8; 1024]),
        };
        let mut dir_buf = vec![0u8; 1024];
        new_dir.write_to(&mut dir_buf);
        write_at(tmp.path(), phys_page_offset(dir_pa, 1024), &dir_buf);

        // Do NOT update page 0 → superblock still says active dir = B = dir_pb.
        let _ = (dir_pb, active); // keep compiler happy
        id
    };

    // Reopen: page 0 says active dir = B (dir_pb), which has the pre-write state.
    let p = Pager::<1024>::open(tmp.path()).unwrap();
    // active_slot = 0 → data_pa is active → zeros.
    assert_eq!(p.protected_active_slot(id), 0);
    assert!(id.get(&p).unwrap().as_bytes().iter().all(|&b| b == 0));
}

#[test]
fn crash_committed_write_survives_reopen() {
    let tmp = TempPath::new();
    let id = {
        let mut p = Pager::<1024>::create(tmp.path()).unwrap();
        let id = p.alloc_protected().unwrap();
        let mut w = id.get_mut(&mut p).unwrap();
        w.page_mut().as_bytes_mut().fill(0xDD);
        w.commit().unwrap();
        id
    };
    let p = Pager::<1024>::open(tmp.path()).unwrap();
    assert!(id.get(&p).unwrap().as_bytes().iter().all(|&b| b == 0xDD));
}

#[test]
fn crash_committed_free_survives_reopen() {
    let tmp = TempPath::new();
    let id = {
        let mut p = Pager::<1024>::create(tmp.path()).unwrap();
        let id = p.alloc_protected().unwrap();
        p.free_protected(id).unwrap();
        id
    };
    let p = Pager::<1024>::open(tmp.path()).unwrap();
    assert!(matches!(id.get(&p), Err(MappedPageError::OutOfBounds)));
}

#[test]
fn crash_corrupt_page0_dir_section_returns_error() {
    let tmp = TempPath::new();
    {
        let mut p = Pager::<1024>::create(tmp.path()).unwrap();
        p.alloc_protected().unwrap(); // writes dir block ref to page 0
    }
    // Corrupt the dir section checksum at the end of page 0.
    zero_range(tmp.path(), 1024 - 4, 4);
    assert!(matches!(
        Pager::<1024>::open(tmp.path()),
        Err(MappedPageError::CorruptDirectoryIndex)
    ));
}

#[test]
fn crash_old_file_without_dir_section_opens_cleanly() {
    // Simulate a file created before protected pages were added:
    // page 0 bytes 20..1024 are all zeros (no dir section, no checksum).
    let tmp = TempPath::new();
    Pager::<1024>::create(tmp.path()).unwrap();
    // Zero out everything after the 20-byte superblock in page 0.
    zero_range(tmp.path(), 20, 1024 - 20);
    // Should open successfully with no directory blocks.
    let p = Pager::<1024>::open(tmp.path()).unwrap();
    assert_eq!(p.free_page_count(), 1);
}

// ── SubPageAllocator tests ────────────────────────────────────────────────────

use crate::{BulkPageAllocator, PageAllocator, PageHandle, SubPageAllocator, SubPageId};

#[test]
fn sub_alloc_basic_alloc_get_free() {
    let tmp = TempPath::new();
    let pager = Pager::<4096>::create(tmp.path()).unwrap();
    let mut sub = SubPageAllocator::<4096, 512>::new(pager);
    let id = sub.alloc().unwrap();
    assert_eq!(id.get(&sub).unwrap().len(), 512);
    id.get_mut(&mut sub).unwrap().as_bytes_mut().fill(0xAB);
    assert!(id.get(&sub).unwrap().as_bytes().iter().all(|&b| b == 0xAB));
    sub.free(id).unwrap();
}

#[test]
fn sub_alloc_fills_one_big_page() {
    // N = 4096 / 512 = 8 sub-slots per big page.
    let tmp = TempPath::new();
    let pager = Pager::<4096>::create(tmp.path()).unwrap();
    let mut sub = SubPageAllocator::<4096, 512>::new(pager);
    // 8 allocs should all come from a single big page.
    let ids: Vec<_> = (0..8).map(|_| sub.alloc().unwrap()).collect();
    let first_slot = ids[0].slot_index;
    assert!(ids.iter().all(|id| id.slot_index == first_slot));
    // 9th alloc must trigger a second big-page allocation.
    let extra = sub.alloc().unwrap();
    assert_ne!(extra.slot_index, first_slot);
    sub.free(extra).unwrap();
    for id in ids {
        sub.free(id).unwrap();
    }
}

#[test]
fn sub_alloc_auto_grow_second_big_page() {
    let tmp = TempPath::new();
    let pager = Pager::<4096>::create(tmp.path()).unwrap();
    let mut sub = SubPageAllocator::<4096, 512>::new(pager);
    let first_ids: Vec<_> = (0..8).map(|_| sub.alloc().unwrap()).collect();
    let id_new = sub.alloc().unwrap();
    assert_ne!(
        id_new.slot_index, first_ids[0].slot_index,
        "9th sub-page must come from a different big-page slot"
    );
    sub.free(id_new).unwrap();
    for id in first_ids {
        sub.free(id).unwrap();
    }
}

#[test]
fn sub_alloc_full_free_returns_big_page() {
    let tmp = TempPath::new();
    let pager = Pager::<4096>::create(tmp.path()).unwrap();
    let mut sub = SubPageAllocator::<4096, 512>::new(pager);
    let free_before = sub.pager().free_page_count();
    let ids: Vec<_> = (0..8).map(|_| sub.alloc().unwrap()).collect();
    // One big page consumed.
    assert!(sub.pager().free_page_count() < free_before + 1);
    // Free all sub-slots; the big page should be returned to the inner pager.
    for id in ids {
        sub.free(id).unwrap();
    }
    assert_eq!(sub.pager().free_page_count(), free_before);
}

#[test]
fn sub_alloc_double_free_error() {
    let tmp = TempPath::new();
    let pager = Pager::<4096>::create(tmp.path()).unwrap();
    let mut sub = SubPageAllocator::<4096, 512>::new(pager);
    let id = sub.alloc().unwrap();
    sub.free(id).unwrap();
    assert!(matches!(sub.free(id), Err(MappedPageError::DoubleFree)));
}

#[test]
fn sub_alloc_out_of_bounds_sub_index() {
    let tmp = TempPath::new();
    let pager = Pager::<4096>::create(tmp.path()).unwrap();
    let mut sub = SubPageAllocator::<4096, 512>::new(pager);
    let _live = sub.alloc().unwrap(); // ensure slot 0 exists
    // sub_index 8 is out of range for N = 8.
    let bad = SubPageId::<4096, 512>::new_raw(0, 8);
    assert!(matches!(bad.get(&sub), Err(MappedPageError::OutOfBounds)));
    assert!(matches!(sub.free(bad), Err(MappedPageError::OutOfBounds)));
}

#[test]
fn sub_alloc_slot_reuse_after_full_free() {
    let tmp = TempPath::new();
    let pager = Pager::<4096>::create(tmp.path()).unwrap();
    let mut sub = SubPageAllocator::<4096, 512>::new(pager);
    let ids: Vec<_> = (0..8).map(|_| sub.alloc().unwrap()).collect();
    let first_slot = ids[0].slot_index;
    for id in ids {
        sub.free(id).unwrap();
    }
    // The tombstone at `first_slot` must be reused by the next alloc.
    let new_id = sub.alloc().unwrap();
    assert_eq!(
        new_id.slot_index, first_slot,
        "tombstone slot must be reused"
    );
}

#[test]
fn sub_alloc_into_pager() {
    let tmp = TempPath::new();
    let pager = Pager::<4096>::create(tmp.path()).unwrap();
    let sub = SubPageAllocator::<4096, 512>::new(pager);
    let pager2 = sub.into_pager();
    assert_eq!(pager2.page_size(), 4096);
}

#[test]
fn sub_alloc_n_equals_64() {
    // PARENT_SIZE / SUB_SIZE = 4096 / 64 = 64 — exercises the full_mask() edge case.
    let tmp = TempPath::new();
    let pager = Pager::<4096>::create(tmp.path()).unwrap();
    let mut sub = SubPageAllocator::<4096, 64>::new(pager);
    let ids: Vec<_> = (0..64).map(|_| sub.alloc().unwrap()).collect();
    assert_eq!(ids.len(), 64);
    // 65th alloc must succeed (new big page).
    let extra = sub.alloc().unwrap();
    assert_ne!(extra.slot_index, ids[0].slot_index);
    sub.free(extra).unwrap();
    for id in ids {
        sub.free(id).unwrap();
    }
}

// ── SubPageAllocator bulk tests ───────────────────────────────────────────────

#[test]
fn sub_bulk_alloc_zero_returns_empty() {
    let tmp = TempPath::new();
    let pager = Pager::<4096>::create(tmp.path()).unwrap();
    let mut sub = SubPageAllocator::<4096, 512>::new(pager);
    let ids = sub.alloc_bulk(0).unwrap();
    assert!(ids.is_empty());
}

#[test]
fn sub_bulk_alloc_returns_distinct_ids() {
    let tmp = TempPath::new();
    let pager = Pager::<4096>::create(tmp.path()).unwrap();
    let mut sub = SubPageAllocator::<4096, 512>::new(pager);
    let ids = sub.alloc_bulk(5).unwrap();
    assert_eq!(ids.len(), 5);
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), 5, "all ids must be distinct");
    for id in ids {
        sub.free(id).unwrap();
    }
}

#[test]
fn sub_bulk_alloc_spans_multiple_big_pages() {
    // 4096 / 512 = 8 sub-slots per big page; alloc 10 forces a second big page.
    let tmp = TempPath::new();
    let pager = Pager::<4096>::create(tmp.path()).unwrap();
    let mut sub = SubPageAllocator::<4096, 512>::new(pager);
    let ids = sub.alloc_bulk(10).unwrap();
    assert_eq!(ids.len(), 10);
    // At least two distinct slot_index values required.
    let distinct_slots: std::collections::HashSet<_> = ids.iter().map(|id| id.slot_index).collect();
    assert!(distinct_slots.len() >= 2);
    for id in ids {
        sub.free(id).unwrap();
    }
}

#[test]
fn sub_bulk_free_zero_is_ok() {
    let tmp = TempPath::new();
    let pager = Pager::<4096>::create(tmp.path()).unwrap();
    let mut sub = SubPageAllocator::<4096, 512>::new(pager);
    assert!(sub.free_bulk(vec![]).is_ok());
}

#[test]
fn sub_bulk_free_releases_pages() {
    let tmp = TempPath::new();
    let pager = Pager::<4096>::create(tmp.path()).unwrap();
    let mut sub = SubPageAllocator::<4096, 512>::new(pager);
    let ids = sub.alloc_bulk(4).unwrap();
    sub.free_bulk(ids).unwrap();
    // After freeing all sub-slots the big page should have been returned;
    // reallocating should succeed and reuse the same slot.
    let id2 = sub.alloc().unwrap();
    assert_eq!(id2.slot_index, 0);
}

#[test]
fn sub_bulk_free_releases_big_page_when_all_sub_slots_freed() {
    // Alloc exactly one full big page worth of sub-slots (8 for 4096/512).
    let tmp = TempPath::new();
    let pager = Pager::<4096>::create(tmp.path()).unwrap();
    let mut sub = SubPageAllocator::<4096, 512>::new(pager);
    let initial_free = sub.pager().free_page_count();
    let ids = sub.alloc_bulk(8).unwrap();
    assert_eq!(sub.pager().free_page_count(), initial_free - 1); // one big page consumed
    sub.free_bulk(ids).unwrap();
    assert_eq!(sub.pager().free_page_count(), initial_free); // big page returned
}

#[test]
fn sub_bulk_free_double_free_error() {
    let tmp = TempPath::new();
    let pager = Pager::<4096>::create(tmp.path()).unwrap();
    let mut sub = SubPageAllocator::<4096, 512>::new(pager);
    let id = sub.alloc().unwrap();
    sub.free(id).unwrap();
    let result = sub.free_bulk(vec![id]);
    assert!(result.is_err());
}

#[test]
fn sub_bulk_free_duplicate_in_input_error() {
    let tmp = TempPath::new();
    let pager = Pager::<4096>::create(tmp.path()).unwrap();
    let mut sub = SubPageAllocator::<4096, 512>::new(pager);
    let id = sub.alloc().unwrap();
    let result = sub.free_bulk(vec![id, id]);
    assert!(result.is_err());
    sub.free(id).unwrap(); // original allocation still valid
}

#[test]
fn sub_bulk_free_out_of_bounds_error() {
    let tmp = TempPath::new();
    let pager = Pager::<4096>::create(tmp.path()).unwrap();
    let mut sub = SubPageAllocator::<4096, 512>::new(pager);
    let bad = SubPageId::<4096, 512>::new_raw(999, 0);
    let result = sub.free_bulk(vec![bad]);
    assert!(result.is_err());
}

#[test]
fn sub_bulk_free_atomic_no_partial_free_on_error() {
    // Alloc two valid ids, then pass them alongside an invalid one.
    // Both valid ids should still be live after the failed bulk free.
    let tmp = TempPath::new();
    let pager = Pager::<4096>::create(tmp.path()).unwrap();
    let mut sub = SubPageAllocator::<4096, 512>::new(pager);
    let id0 = sub.alloc().unwrap();
    let id1 = sub.alloc().unwrap();
    let bad = SubPageId::<4096, 512>::new_raw(999, 0);
    let result = sub.free_bulk(vec![id0, id1, bad]);
    assert!(result.is_err());
    // Both ids must still be individually freeable.
    sub.free(id0).unwrap();
    sub.free(id1).unwrap();
}

// ── alloc_bulk / free_bulk tests ──────────────────────────────────────────────

#[test]
fn bulk_alloc_zero_returns_empty() {
    let tmp = TempPath::new();
    let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
    let ids = pager.alloc_bulk(0).unwrap();
    assert!(ids.is_empty());
}

#[test]
fn bulk_alloc_returns_distinct_ids() {
    let tmp = TempPath::new();
    let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
    let ids = pager.alloc_bulk(3).unwrap();
    assert_eq!(ids.len(), 3);
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), 3, "all ids must be distinct");
    for id in &ids {
        assert!(id.0 >= FIRST_DATA_PAGE);
    }
}

#[test]
fn bulk_alloc_all_data_pages() {
    let tmp = TempPath::new();
    let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
    // Initial file: 4 pages total, 1 free data page (page 3).
    let ids = pager.alloc_bulk(1).unwrap();
    assert_eq!(ids.len(), 1);
    assert_eq!(ids[0].0, FIRST_DATA_PAGE);
}

#[test]
fn bulk_alloc_triggers_grow() {
    let tmp = TempPath::new();
    let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
    // Exhaust the initial free page, then allocate 5 more (forces grow).
    let ids = pager.alloc_bulk(6).unwrap();
    assert_eq!(ids.len(), 6);
}

#[test]
fn bulk_alloc_persists_across_reopen() {
    let tmp = TempPath::new();
    let ids = {
        let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
        pager.alloc_bulk(3).unwrap()
    };
    let pager = Pager::<4096>::open(tmp.path()).unwrap();
    assert_eq!(
        pager.free_page_count() + 3,
        pager.page_count() - FIRST_DATA_PAGE
    );
    // All allocated ids should be accessible.
    for id in &ids {
        assert!(id.get(&pager).is_ok());
    }
}

#[test]
fn bulk_free_zero_is_ok() {
    let tmp = TempPath::new();
    let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
    assert!(pager.free_bulk(vec![]).is_ok());
}

#[test]
fn bulk_free_releases_pages() {
    let tmp = TempPath::new();
    let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
    let ids = pager.alloc_bulk(3).unwrap();
    let free_before = pager.free_page_count();
    pager.free_bulk(ids).unwrap();
    assert_eq!(pager.free_page_count(), free_before + 3);
}

#[test]
fn bulk_free_reserved_page_error() {
    let tmp = TempPath::new();
    let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
    let result = pager.free_bulk(vec![PageId(0)]);
    assert!(matches!(result, Err(MappedPageError::ReservedPage)));
}

#[test]
fn bulk_free_out_of_bounds_error() {
    let tmp = TempPath::new();
    let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
    let result = pager.free_bulk(vec![PageId(9999)]);
    assert!(matches!(result, Err(MappedPageError::OutOfBounds)));
}

#[test]
fn bulk_free_double_free_error() {
    let tmp = TempPath::new();
    let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
    let ids = pager.alloc_bulk(2).unwrap();
    pager.free_bulk(ids.clone()).unwrap();
    // Freeing the same pages again should fail.
    let result = pager.free_bulk(ids);
    assert!(matches!(result, Err(MappedPageError::DoubleFree)));
}

#[test]
fn bulk_free_duplicate_in_input_error() {
    let tmp = TempPath::new();
    let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
    let ids = pager.alloc_bulk(1).unwrap();
    // Pass the same PageId twice.
    let result = pager.free_bulk(vec![ids[0], ids[0]]);
    assert!(matches!(result, Err(MappedPageError::DoubleFree)));
    // The page must still be allocated (no partial state change).
    assert!(ids[0].get(&pager).is_ok());
}

#[test]
fn bulk_free_atomic_no_partial_free_on_error() {
    let tmp = TempPath::new();
    let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
    let ids = pager.alloc_bulk(2).unwrap();
    let free_before = pager.free_page_count();
    // Mix a valid id with an out-of-bounds one.
    let result = pager.free_bulk(vec![ids[0], PageId(9999)]);
    assert!(result.is_err());
    // No pages should have been freed.
    assert_eq!(pager.free_page_count(), free_before);
}

#[test]
fn bulk_free_persists_across_reopen() {
    let tmp = TempPath::new();
    let ids = {
        let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
        let ids = pager.alloc_bulk(3).unwrap();
        pager.free_bulk(ids.clone()).unwrap();
        ids
    };
    let pager = Pager::<4096>::open(tmp.path()).unwrap();
    // Freed pages should be available again; trying to get them should fail as unallocated
    // (they're free so get_page would succeed if bounds are ok — verify free_count instead).
    assert_eq!(
        pager.free_page_count(),
        pager.page_count() - FIRST_DATA_PAGE,
        "all data pages should be free after reopen"
    );
    drop(ids);
}

// ── iter_allocated_pages tests ────────────────────────────────────────────────

#[test]
fn iter_empty_pager_yields_nothing() {
    let tmp = TempPath::new();
    let pager = Pager::<4096>::create(tmp.path()).unwrap();
    let allocated: Vec<_> = pager.iter_allocated_pages().collect();
    assert!(allocated.is_empty());
}

#[test]
fn iter_yields_allocated_pages() {
    let tmp = TempPath::new();
    let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
    let ids = pager.alloc_bulk(3).unwrap();
    let mut iterated: Vec<_> = pager.iter_allocated_pages().collect();
    iterated.sort_unstable();
    let mut expected = ids.clone();
    expected.sort_unstable();
    assert_eq!(iterated, expected);
}

#[test]
fn iter_excludes_freed_pages() {
    let tmp = TempPath::new();
    let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
    let ids = pager.alloc_bulk(3).unwrap();
    pager.free(ids[1]).unwrap();
    let iterated: Vec<_> = pager.iter_allocated_pages().collect();
    assert_eq!(iterated.len(), 2);
    assert!(!iterated.contains(&ids[1]));
}

#[test]
fn iter_excludes_reserved_pages() {
    let tmp = TempPath::new();
    let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
    pager.alloc_bulk(2).unwrap();
    for id in pager.iter_allocated_pages() {
        assert!(
            id.0 >= FIRST_DATA_PAGE,
            "reserved page leaked through iterator"
        );
    }
}

#[test]
fn iter_count_matches_alloc_count() {
    let tmp = TempPath::new();
    let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
    pager.alloc_bulk(5).unwrap();
    let count = pager.iter_allocated_pages().count();
    assert_eq!(count, 5);
}

#[test]
fn iter_after_free_bulk_is_empty() {
    let tmp = TempPath::new();
    let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
    let ids = pager.alloc_bulk(4).unwrap();
    pager.free_bulk(ids).unwrap();
    let iterated: Vec<_> = pager.iter_allocated_pages().collect();
    assert!(iterated.is_empty());
}

#[test]
fn iter_survives_reopen() {
    let tmp = TempPath::new();
    let ids = {
        let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
        pager.alloc_bulk(3).unwrap()
    };
    let pager = Pager::<4096>::open(tmp.path()).unwrap();
    let mut iterated: Vec<_> = pager.iter_allocated_pages().collect();
    iterated.sort_unstable();
    let mut expected = ids.clone();
    expected.sort_unstable();
    assert_eq!(iterated, expected);
}

// ── iter_allocated_pages / iter_allocated_protected_pages separation tests ────

#[test]
fn iter_regular_excludes_protected_backing_pages() {
    let tmp = TempPath::new();
    let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
    // Allocate a protected page — this reserves physical backing pages.
    let _pid = pager.alloc_protected().unwrap();
    // Regular iterator must yield nothing (no regular pages allocated).
    let regular: Vec<_> = pager.iter_allocated_pages().collect();
    assert!(
        regular.is_empty(),
        "regular iterator yielded a protected backing page: {regular:?}"
    );
}

#[test]
fn iter_regular_excludes_dir_block_pages() {
    let tmp = TempPath::new();
    let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
    let _pid = pager.alloc_protected().unwrap();
    // All allocated physical pages should be internal; none regular.
    let regular: Vec<_> = pager.iter_allocated_pages().collect();
    assert!(
        regular.is_empty(),
        "regular iterator exposed dir-block pages: {regular:?}"
    );
}

#[test]
fn iter_regular_and_protected_together_are_disjoint() {
    let tmp = TempPath::new();
    let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
    let reg_id = pager.alloc().unwrap();
    let prot_id = pager.alloc_protected().unwrap();

    let regular: Vec<_> = pager.iter_allocated_pages().collect();
    let protected: Vec<_> = pager.iter_allocated_protected_pages().collect();

    assert_eq!(regular, vec![reg_id], "regular iterator mismatch");
    assert_eq!(protected, vec![prot_id], "protected iterator mismatch");

    // No physical page id should appear in both sets.
    for r in &regular {
        let prot_phys = pager.protected_backing_pages(prot_id);
        assert!(
            r.0 != prot_phys.0 && r.0 != prot_phys.1,
            "regular page {r:?} overlaps protected backing pages"
        );
    }
}

#[test]
fn iter_protected_empty_when_no_protected_pages() {
    let tmp = TempPath::new();
    let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
    pager.alloc_bulk(3).unwrap();
    let protected: Vec<_> = pager.iter_allocated_protected_pages().collect();
    assert!(
        protected.is_empty(),
        "protected iterator should be empty: {protected:?}"
    );
}

#[test]
fn iter_protected_yields_in_use_slots_only() {
    let tmp = TempPath::new();
    let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
    let p0 = pager.alloc_protected().unwrap();
    let p1 = pager.alloc_protected().unwrap();
    let p2 = pager.alloc_protected().unwrap();
    // Free the middle one.
    pager.free_protected(p1).unwrap();
    let mut iterated: Vec<_> = pager.iter_allocated_protected_pages().collect();
    iterated.sort_unstable();
    assert_eq!(iterated, vec![p0, p2]);
}

#[test]
fn iter_protected_count_matches_alloc_count() {
    let tmp = TempPath::new();
    let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
    pager.alloc_protected().unwrap();
    pager.alloc_protected().unwrap();
    pager.alloc_protected().unwrap();
    assert_eq!(pager.iter_allocated_protected_pages().count(), 3);
}

#[test]
fn iter_protected_empty_after_free_all() {
    let tmp = TempPath::new();
    let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
    let ids = pager.alloc_protected_bulk(3).unwrap();
    pager.free_protected_bulk(ids).unwrap();
    assert!(
        pager
            .iter_allocated_protected_pages()
            .collect::<Vec<_>>()
            .is_empty()
    );
}

#[test]
fn iter_protected_survives_reopen() {
    let tmp = TempPath::new();
    let ids = {
        let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
        pager.alloc_protected_bulk(2).unwrap()
    };
    let pager = Pager::<4096>::open(tmp.path()).unwrap();
    let mut iterated: Vec<_> = pager.iter_allocated_protected_pages().collect();
    iterated.sort_unstable();
    let mut expected = ids.clone();
    expected.sort_unstable();
    assert_eq!(iterated, expected);
}

// ── alloc_protected_bulk / free_protected_bulk tests ─────────────────────────

#[test]
fn bulk_protected_alloc_zero_returns_empty() {
    let tmp = TempPath::new();
    let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
    assert!(pager.alloc_protected_bulk(0).unwrap().is_empty());
}

#[test]
fn bulk_protected_alloc_returns_distinct_ids() {
    let tmp = TempPath::new();
    let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
    let ids = pager.alloc_protected_bulk(3).unwrap();
    assert_eq!(ids.len(), 3);
    let mut sorted = ids.clone();
    sorted.dedup();
    assert_eq!(sorted.len(), 3, "all ids must be distinct");
}

#[test]
fn bulk_protected_alloc_pages_are_accessible() {
    let tmp = TempPath::new();
    let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
    let ids = pager.alloc_protected_bulk(3).unwrap();
    for id in &ids {
        assert!(id.get(&pager).is_ok());
    }
}

#[test]
fn bulk_protected_alloc_persists_across_reopen() {
    let tmp = TempPath::new();
    let ids = {
        let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
        pager.alloc_protected_bulk(2).unwrap()
    };
    let pager = Pager::<4096>::open(tmp.path()).unwrap();
    for id in &ids {
        assert!(id.get(&pager).is_ok());
    }
}

#[test]
fn bulk_protected_free_zero_is_ok() {
    let tmp = TempPath::new();
    let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
    assert!(pager.free_protected_bulk(vec![]).is_ok());
}

#[test]
fn bulk_protected_free_releases_pages() {
    let tmp = TempPath::new();
    let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
    let ids = pager.alloc_protected_bulk(3).unwrap();
    let free_before = pager.free_page_count();
    pager.free_protected_bulk(ids).unwrap();
    // Each protected page frees 2 backing pages.
    assert_eq!(pager.free_page_count(), free_before + 6);
}

#[test]
fn bulk_protected_free_double_free_error() {
    let tmp = TempPath::new();
    let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
    let ids = pager.alloc_protected_bulk(2).unwrap();
    pager.free_protected_bulk(ids.clone()).unwrap();
    let result = pager.free_protected_bulk(ids);
    assert!(matches!(result, Err(MappedPageError::DoubleFree)));
}

#[test]
fn bulk_protected_free_duplicate_in_input_error() {
    let tmp = TempPath::new();
    let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
    let ids = pager.alloc_protected_bulk(1).unwrap();
    let result = pager.free_protected_bulk(vec![ids[0], ids[0]]);
    assert!(matches!(result, Err(MappedPageError::DoubleFree)));
    // Page must still be allocated.
    assert!(ids[0].get(&pager).is_ok());
}

#[test]
fn bulk_protected_free_out_of_bounds_error() {
    let tmp = TempPath::new();
    let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
    let result = pager.free_protected_bulk(vec![ProtectedPageId(9999)]);
    assert!(matches!(result, Err(MappedPageError::OutOfBounds)));
}

#[test]
fn bulk_protected_free_atomic_no_partial_free_on_error() {
    let tmp = TempPath::new();
    let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
    let ids = pager.alloc_protected_bulk(2).unwrap();
    let free_before = pager.free_page_count();
    // Mix a valid id with an out-of-bounds one.
    let result = pager.free_protected_bulk(vec![ids[0], ProtectedPageId(9999)]);
    assert!(result.is_err());
    assert_eq!(
        pager.free_page_count(),
        free_before,
        "no pages should have been freed"
    );
}

// ── ConcurrentPager tests ─────────────────────────────────────────────────────

#[test]
fn concurrent_new_and_clone_share_pager() {
    let tmp = TempPath::new();
    let pager = Pager::<4096>::create(tmp.path()).unwrap();
    let shared = ConcurrentPager::new(pager);
    let clone = shared.clone();

    // Allocate through the original, read through the clone.
    let id = shared.write().unwrap().alloc().unwrap();
    clone
        .write()
        .unwrap()
        .get_page(id)
        .expect("page allocated through shared is visible via clone");
}

#[test]
fn concurrent_read_guard_deref_gives_pager() {
    let tmp = TempPath::new();
    let pager = Pager::<4096>::create(tmp.path()).unwrap();
    let shared = ConcurrentPager::new(pager);

    let id = shared.write().unwrap().alloc().unwrap();
    let guard = shared.read().unwrap();
    // The read guard should deref to &Pager and expose get_page.
    assert!(id.get(&*guard).is_ok());
}

#[test]
fn concurrent_write_guard_deref_gives_mut_pager() {
    let tmp = TempPath::new();
    let pager = Pager::<4096>::create(tmp.path()).unwrap();
    let shared = ConcurrentPager::new(pager);

    let mut guard = shared.write().unwrap();
    let id = guard.alloc().unwrap();
    // Mutable deref: write bytes then verify via shared read.
    id.get_mut(&mut *guard).unwrap().as_bytes_mut()[0] = 42;
    drop(guard);

    assert_eq!(
        shared.read().unwrap().get_page(id).unwrap().as_bytes()[0],
        42
    );
}

#[test]
fn concurrent_multiple_readers_simultaneously() {
    let tmp = TempPath::new();
    let pager = Pager::<4096>::create(tmp.path()).unwrap();
    let shared = ConcurrentPager::new(pager);

    // Allocate a page and write a known byte.
    let id = {
        let mut w = shared.write().unwrap();
        let id = w.alloc().unwrap();
        w.get_page_mut(id).unwrap().as_bytes_mut()[0] = 99;
        id
    };

    // Spawn N threads, each acquiring a read guard and checking the byte.
    let shared = Arc::new(shared);
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let s = Arc::clone(&shared);
            thread::spawn(move || {
                let guard = s.read().unwrap();
                guard.get_page(id).unwrap().as_bytes()[0]
            })
        })
        .collect();

    for h in handles {
        assert_eq!(h.join().unwrap(), 99);
    }
}

#[test]
fn concurrent_write_then_read_across_threads() {
    let tmp = TempPath::new();
    let pager = Pager::<4096>::create(tmp.path()).unwrap();
    let shared = Arc::new(ConcurrentPager::new(pager));

    // Writer thread: allocate a page, write a sentinel.
    let writer = Arc::clone(&shared);
    let id = thread::spawn(move || {
        let mut guard = writer.write().unwrap();
        let id = guard.alloc().unwrap();
        guard.get_page_mut(id).unwrap().as_bytes_mut()[0] = 77;
        id
    })
    .join()
    .unwrap();

    // Reader thread: verify the sentinel is visible.
    let reader = Arc::clone(&shared);
    let byte = thread::spawn(move || {
        let guard = reader.read().unwrap();
        guard.get_page(id).unwrap().as_bytes()[0]
    })
    .join()
    .unwrap();

    assert_eq!(byte, 77);
}

#[test]
fn concurrent_try_write_returns_would_block_while_read_held() {
    let tmp = TempPath::new();
    let pager = Pager::<4096>::create(tmp.path()).unwrap();
    let shared = ConcurrentPager::new(pager);

    let _read_guard = shared.read().unwrap();
    let result = shared.try_write();
    assert!(
        matches!(result, Err(ConcurrentPagerError::WouldBlock)),
        "try_write must fail while a read guard is held"
    );
}

#[test]
fn concurrent_try_read_returns_would_block_while_write_held() {
    let tmp = TempPath::new();
    let pager = Pager::<4096>::create(tmp.path()).unwrap();
    let shared = ConcurrentPager::new(pager);

    let _write_guard = shared.write().unwrap();
    let result = shared.try_read();
    assert!(
        matches!(result, Err(ConcurrentPagerError::WouldBlock)),
        "try_read must fail while a write guard is held"
    );
}

#[test]
fn concurrent_into_inner_returns_some_when_sole_owner() {
    let tmp = TempPath::new();
    let pager = Pager::<4096>::create(tmp.path()).unwrap();
    let shared = ConcurrentPager::new(pager);
    assert!(shared.into_inner().is_some());
}

#[test]
fn concurrent_into_inner_returns_none_when_clones_exist() {
    let tmp = TempPath::new();
    let pager = Pager::<4096>::create(tmp.path()).unwrap();
    let shared = ConcurrentPager::new(pager);
    let _clone = shared.clone();
    assert!(shared.into_inner().is_none());
}

#[test]
fn concurrent_from_impl() {
    let tmp = TempPath::new();
    let pager = Pager::<4096>::create(tmp.path()).unwrap();
    let shared: ConcurrentPager<4096> = pager.into();
    assert!(shared.read().is_ok());
}

#[test]
fn concurrent_protected_page_read_across_threads() {
    let tmp = TempPath::new();
    let pager = Pager::<4096>::create(tmp.path()).unwrap();
    let shared = Arc::new(ConcurrentPager::new(pager));

    // Allocate and write a protected page on the main thread.
    let pid = {
        let mut guard = shared.write().unwrap();
        let pid = guard.alloc_protected().unwrap();
        let mut writer = pid.get_mut(&mut *guard).unwrap();
        writer.page_mut().as_bytes_mut()[0] = 55;
        writer.commit().unwrap();
        pid
    };

    // Verify from a worker thread via read guard.
    let s = Arc::clone(&shared);
    let byte = thread::spawn(move || {
        let guard = s.read().unwrap();
        pid.get(&*guard).unwrap().as_bytes()[0]
    })
    .join()
    .unwrap();

    assert_eq!(byte, 55);
}

// ── ReadOnlyPager tests ───────────────────────────────────────────────────────

#[test]
fn readonly_open_sees_allocated_pages() {
    use crate::ReadOnlyPager;

    let tmp = TempPath::new();
    let id = {
        let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
        let id = pager.alloc().unwrap();
        pager.get_page_mut(id).unwrap().as_bytes_mut()[0] = 42;
        id
    };

    let ro = ReadOnlyPager::<4096>::open(tmp.path()).unwrap();
    assert_eq!(ro.page_count(), 4);
    let pages: Vec<_> = ro.iter_allocated_pages().collect();
    assert_eq!(pages, vec![id]);
    assert_eq!(ro.get_page(id).unwrap().as_bytes()[0], 42);
}

#[test]
fn readonly_wrong_page_size_is_rejected() {
    use crate::ReadOnlyPager;

    let tmp = TempPath::new();
    Pager::<4096>::create(tmp.path()).unwrap();
    assert!(matches!(
        ReadOnlyPager::<1024>::open(tmp.path()),
        Err(MappedPageError::InvalidPageSize)
    ));
}

#[test]
fn readonly_reserved_page_returns_error() {
    use crate::ReadOnlyPager;

    let tmp = TempPath::new();
    Pager::<4096>::create(tmp.path()).unwrap();
    let ro = ReadOnlyPager::<4096>::open(tmp.path()).unwrap();
    assert!(matches!(
        ro.get_page(PageId(0)),
        Err(MappedPageError::ReservedPage)
    ));
    assert!(matches!(
        ro.get_page(PageId(1)),
        Err(MappedPageError::ReservedPage)
    ));
    assert!(matches!(
        ro.get_page(PageId(2)),
        Err(MappedPageError::ReservedPage)
    ));
}

#[test]
fn readonly_out_of_bounds_returns_error() {
    use crate::ReadOnlyPager;

    let tmp = TempPath::new();
    Pager::<4096>::create(tmp.path()).unwrap();
    let ro = ReadOnlyPager::<4096>::open(tmp.path()).unwrap();
    assert!(matches!(
        ro.get_page(PageId(999)),
        Err(MappedPageError::OutOfBounds)
    ));
}

#[test]
fn readonly_sees_protected_pages() {
    use crate::ReadOnlyPager;

    let tmp = TempPath::new();
    let pid = {
        let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
        let pid = pager.alloc_protected().unwrap();
        let mut w = pid.get_mut(&mut pager).unwrap();
        w.page_mut().as_bytes_mut()[0] = 99;
        w.commit().unwrap();
        pid
    };

    let ro = ReadOnlyPager::<4096>::open(tmp.path()).unwrap();
    let protected: Vec<_> = ro.iter_allocated_protected_pages().collect();
    assert_eq!(protected, vec![pid]);
    assert_eq!(ro.get_protected_page(pid).unwrap().as_bytes()[0], 99);
}

#[test]
fn readonly_iter_excludes_internal_pages() {
    use crate::ReadOnlyPager;

    let tmp = TempPath::new();
    {
        let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
        pager.alloc_protected().unwrap();
        pager.alloc().unwrap();
    }

    let ro = ReadOnlyPager::<4096>::open(tmp.path()).unwrap();
    // iter_allocated_pages must yield only the regular data page, not the two
    // protected backing pages or the two directory block pages.
    assert_eq!(ro.iter_allocated_pages().count(), 1);
    assert_eq!(ro.iter_allocated_protected_pages().count(), 1);
}

#[test]
fn readonly_metadata_agrees_with_writer() {
    use crate::ReadOnlyPager;

    let tmp = TempPath::new();
    {
        let mut pager = Pager::<4096>::create(tmp.path()).unwrap();
        pager.alloc().unwrap();
        pager.alloc().unwrap();
    }

    let pager = Pager::<4096>::open(tmp.path()).unwrap();
    let ro = ReadOnlyPager::<4096>::open(tmp.path()).unwrap();

    assert_eq!(ro.page_count(), pager.page_count());
    assert_eq!(ro.free_page_count(), pager.free_page_count());
    assert_eq!(ro.page_size(), pager.page_size());
}
