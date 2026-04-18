//! The `Pager`: file + mmap management, allocation, and crash-safe commits.

use std::fs::{File, OpenOptions};
use std::path::Path;

use memmap2::MmapMut;

use crate::error::MappedPageError;
use crate::meta::{
    MetaPage, MetaSelector, Superblock, FIRST_DATA_PAGE, MAGIC, MIN_PAGE_SIZE_LOG2,
};
use crate::page::{MappedPage, PageId};

/// Manages a memory-mapped, fixed-size-page file.
///
/// # Crash consistency
///
/// Every state-changing operation (alloc, free, grow) writes updated metadata
/// to the *inactive* metadata page, fsyncs it, then flips the active pointer
/// in the superblock and fsyncs the superblock.  The active page is never
/// overwritten in place.
///
/// # Reference invalidation
///
/// After a grow the file is remapped.  Any `&MappedPage` or `&mut MappedPage`
/// obtained before the grow is **invalid** after it returns.  The borrow
/// checker enforces this: all page references hold a borrow on `&Pager` or
/// `&mut Pager`, so `alloc` (which requires `&mut Pager`) cannot be called
/// while any reference is live.
///
/// # Unavailable state
///
/// If a grow operation fails after extending the file but before re-mapping,
/// `self.mmap` becomes `None`.  All subsequent operations on the pager return
/// `MappedPageError::Unavailable`.  The file on disk is still consistent and
/// can be reopened via `Pager::open`.
pub struct Pager {
    file: File,
    /// `None` only after a failed remap; all operations return `Unavailable`.
    mmap: Option<MmapMut>,
    page_size: usize,
    active_meta: MetaSelector,
    /// In-memory working copy of the active metadata.
    meta: MetaPage,
}

impl Pager {
    // ── Construction ──────────────────────────────────────────────────────────

    /// Create a new pager backed by `path`.
    ///
    /// `page_size_log2` sets page size to `2^page_size_log2` bytes and must be
    /// at least `MIN_PAGE_SIZE_LOG2` (10, i.e. 1024 bytes).
    /// The file must not already exist.
    pub fn create(path: impl AsRef<Path>, page_size_log2: u32) -> Result<Self, MappedPageError> {
        if page_size_log2 < MIN_PAGE_SIZE_LOG2 {
            return Err(MappedPageError::InvalidPageSize);
        }
        let page_size = 1usize << page_size_log2;
        let initial_pages = 4u64;

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;

        file.set_len(initial_pages * page_size as u64)?;

        let mut mmap = unsafe { MmapMut::map_mut(&file) }?;

        let meta = MetaPage::new_for_capacity(initial_pages);

        // Serialize metadata; write to both page 1 (A) and page 2 (B).
        let mut meta_buf = vec![0u8; page_size];
        meta.write_to(&mut meta_buf);
        mmap[page_size..2 * page_size].copy_from_slice(&meta_buf);
        mmap[2 * page_size..3 * page_size].copy_from_slice(&meta_buf);

        // Write superblock with A as active.
        let meta_checksum = MetaPage::page_checksum(&meta_buf);
        let sb = Superblock {
            magic: MAGIC,
            page_size_log2,
            active_meta: MetaSelector::A,
            meta_checksum,
        };
        let mut sb_buf = [0u8; 20];
        sb.write_to(&mut sb_buf);
        mmap[0..20].copy_from_slice(&sb_buf);

        mmap.flush()?;

        Ok(Pager { file, mmap: Some(mmap), page_size, active_meta: MetaSelector::A, meta })
    }

    /// Open an existing pager file, validating and recovering metadata.
    ///
    /// The superblock is read first; from it we learn the page size and which
    /// metadata page is active.  The active page is then validated against both
    /// its own embedded checksum and the superblock's `meta_checksum`.  If it
    /// fails, the alternate is tried (internal checksum only).  Both failing is
    /// an error.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, MappedPageError> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;

        let mmap = unsafe { MmapMut::map_mut(&file) }?;

        if mmap.len() < 20 {
            return Err(MappedPageError::CorruptSuperblock);
        }

        let sb = Superblock::from_bytes(&mmap[0..20])
            .filter(|sb| sb.is_valid())
            .ok_or(MappedPageError::CorruptSuperblock)?;

        let page_size = (1usize)
            .checked_shl(sb.page_size_log2)
            .filter(|&ps| ps >= (1 << MIN_PAGE_SIZE_LOG2))
            .ok_or(MappedPageError::InvalidPageSize)?;

        if mmap.len() < 4 * page_size {
            return Err(MappedPageError::CorruptSuperblock);
        }

        let active = sb.active_meta;
        let alt = active.other();

        // Try the superblock-designated active page: internal checksum + superblock checksum.
        let active_opt: Option<MetaPage> = {
            let off = active.page_id() as usize * page_size;
            let page = &mmap[off..off + page_size];
            MetaPage::from_bytes(page)
                .filter(|_| MetaPage::page_checksum(page) == sb.meta_checksum)
        };

        // Fall back to the alternate page: internal checksum only.
        let (meta, active_meta) = if let Some(m) = active_opt {
            (m, active)
        } else {
            let off = alt.page_id() as usize * page_size;
            let page = &mmap[off..off + page_size];
            let m = MetaPage::from_bytes(page).ok_or(MappedPageError::CorruptMetadata)?;
            (m, alt)
        };

        Ok(Pager { file, mmap: Some(mmap), page_size, active_meta, meta })
    }

    // ── Allocation ────────────────────────────────────────────────────────────

    /// Allocate a fresh page.  Grows the file if no free pages remain.
    ///
    /// Pages 0–2 are never returned.
    pub fn alloc(&mut self) -> Result<PageId, MappedPageError> {
        if let Some(id) = self.meta.alloc_page() {
            self.commit()?;
            return Ok(PageId(id));
        }
        // No free pages: grow then retry.
        self.grow()?;
        let id = self.meta.alloc_page().expect("grow always adds free pages");
        self.commit()?;
        Ok(PageId(id))
    }

    /// Mark `id` as free so it can be returned by a future `alloc`.
    ///
    /// Returns an error if `id` is reserved (0–2), out of range, or already free.
    pub fn free(&mut self, id: PageId) -> Result<(), MappedPageError> {
        if id.0 < FIRST_DATA_PAGE {
            return Err(MappedPageError::ReservedPage);
        }
        if !self.meta.free_page(id.0) {
            // free_page returns false for out-of-range OR already-free.
            return Err(if id.0 >= self.meta.total_pages {
                MappedPageError::OutOfBounds
            } else {
                MappedPageError::DoubleFree
            });
        }
        self.commit()
    }

    // ── Metadata accessors ────────────────────────────────────────────────────

    /// The page size this pager was created with, in bytes.
    pub fn page_size(&self) -> usize {
        self.page_size
    }

    /// Total number of pages in the file, including reserved pages 0–2.
    pub fn page_count(&self) -> u64 {
        self.meta.total_pages
    }

    /// Number of pages currently available for allocation.
    pub fn free_page_count(&self) -> u64 {
        self.meta.free_count
    }

    /// Which metadata selector is currently active (test-only introspection).
    #[cfg(test)]
    pub(crate) fn active_meta_selector(&self) -> MetaSelector {
        self.active_meta
    }

    // ── Page access (called by PageId) ────────────────────────────────────────

    pub(crate) fn get_page(&self, id: PageId) -> Result<&MappedPage, MappedPageError> {
        if id.0 < FIRST_DATA_PAGE {
            return Err(MappedPageError::ReservedPage);
        }
        if id.0 >= self.meta.total_pages {
            return Err(MappedPageError::OutOfBounds);
        }
        let off = id.0 as usize * self.page_size;
        let ps = self.page_size;
        let slice = &self.mmap()?[off..off + ps];
        Ok(unsafe { MappedPage::from_slice(slice) })
    }

    pub(crate) fn get_page_mut(&mut self, id: PageId) -> Result<&mut MappedPage, MappedPageError> {
        if id.0 < FIRST_DATA_PAGE {
            return Err(MappedPageError::ReservedPage);
        }
        if id.0 >= self.meta.total_pages {
            return Err(MappedPageError::OutOfBounds);
        }
        let off = id.0 as usize * self.page_size;
        let ps = self.page_size;
        let slice = &mut self.mmap_mut()?[off..off + ps];
        Ok(unsafe { MappedPage::from_slice_mut(slice) })
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    fn mmap(&self) -> Result<&MmapMut, MappedPageError> {
        self.mmap.as_ref().ok_or(MappedPageError::Unavailable)
    }

    fn mmap_mut(&mut self) -> Result<&mut MmapMut, MappedPageError> {
        self.mmap.as_mut().ok_or(MappedPageError::Unavailable)
    }

    /// Double-buffered commit:
    /// 1. Serialize `self.meta` into the *inactive* metadata page and msync it.
    /// 2. Rewrite the superblock to point to the inactive page and msync it.
    /// 3. Flip `self.active_meta`.
    fn commit(&mut self) -> Result<(), MappedPageError> {
        let inactive = self.active_meta.other();
        let inactive_off = inactive.page_id() as usize * self.page_size;
        let ps = self.page_size;

        self.meta.generation += 1;

        // Serialize into a temp buffer so we can compute the checksum without
        // a second mmap borrow, and avoid borrow conflicts when writing.
        let mut meta_buf = vec![0u8; ps];
        self.meta.write_to(&mut meta_buf);
        let meta_checksum = MetaPage::page_checksum(&meta_buf);

        // Step 1: write metadata to inactive page, then msync.
        self.mmap_mut()?[inactive_off..inactive_off + ps].copy_from_slice(&meta_buf);
        self.mmap()?.flush_range(inactive_off, ps)?;

        // Step 2: write updated superblock, then msync page 0.
        let sb = Superblock {
            magic: MAGIC,
            page_size_log2: self.page_size.trailing_zeros(),
            active_meta: inactive,
            meta_checksum,
        };
        let mut sb_buf = [0u8; 20];
        sb.write_to(&mut sb_buf);
        self.mmap_mut()?[0..20].copy_from_slice(&sb_buf);
        self.mmap()?.flush_range(0, ps)?;

        // Step 3: commit is durable; update in-memory pointer.
        self.active_meta = inactive;
        Ok(())
    }

    /// Extend the file to twice its current page count and remap.
    ///
    /// Does not commit; the caller (`alloc`) allocates a page and commits once.
    ///
    /// If `set_len` fails the original mmap is restored and the error is returned.
    /// If `map_mut` fails after a successful `set_len`, the mmap becomes `None`
    /// (`Unavailable`); the file is consistent and can be reopened.
    fn grow(&mut self) -> Result<(), MappedPageError> {
        let new_total = self.meta.total_pages * 2;
        let old_file_size = self.meta.total_pages * self.page_size as u64;
        let new_file_size = new_total * self.page_size as u64;

        // Drop the mmap before resizing; required on all platforms.
        drop(self.mmap.take());

        if let Err(e) = self.file.set_len(new_file_size) {
            // File size unchanged; restore the mapping at original size.
            self.mmap = Some(unsafe { MmapMut::map_mut(&self.file) }
                .map_err(MappedPageError::Io)?);
            return Err(MappedPageError::Io(e));
        }

        match unsafe { MmapMut::map_mut(&self.file) } {
            Ok(m) => {
                self.mmap = Some(m);
                self.meta.grow_to(new_total);
                Ok(())
            }
            Err(e) => {
                // Best-effort rollback of the file extension; mmap stays None.
                let _ = self.file.set_len(old_file_size);
                Err(MappedPageError::Io(e))
            }
        }
    }
}
