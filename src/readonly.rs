//! Read-only pager: maps a `mappedpages` file with a read-only `Mmap`.
//!
//! [`ReadOnlyPager<PAGE_SIZE>`] opens an existing file without write access.
//! All write operations (alloc, free, page mutation) are absent from this type
//! and therefore compile-time errors for callers.  Multiple processes may open
//! the same file as `ReadOnlyPager` simultaneously; reads never modify the file.

use std::fs::{File, OpenOptions};
use std::path::Path;

use memmap2::Mmap;

use crate::error::MappedPageError;
use crate::meta::{
    DirBlockRef, DirEntry, DirPage, FIRST_DATA_PAGE, MetaPage, MetaSelector, Superblock,
    dir_entries_per_page, read_dir_blocks,
};
use crate::page::{MappedPage, PageId};
use crate::protected::ProtectedPageId;

/// A read-only view of a memory-mapped, fixed-size-page file.
///
/// Opened with [`ReadOnlyPager::open`].  Because no write methods exist on this
/// type, callers cannot allocate, free, or mutate pages — attempts to do so are
/// compile-time errors.
///
/// # Concurrent access
///
/// The underlying [`Mmap`](memmap2::Mmap) is `Send + Sync`, so `ReadOnlyPager`
/// is automatically `Send + Sync`.  Multiple threads — or multiple processes —
/// may open the same file as `ReadOnlyPager` concurrently without coordination.
pub struct ReadOnlyPager<const PAGE_SIZE: usize> {
    _file: File,
    mmap: Mmap,
    meta: MetaPage,
    dir_blocks: Vec<DirBlockRef>,
    dir_pages: Vec<DirPage>,
}

impl<const PAGE_SIZE: usize> ReadOnlyPager<PAGE_SIZE> {
    // ── Construction ──────────────────────────────────────────────────────────

    /// Open an existing `mappedpages` file in read-only mode.
    ///
    /// Validates the superblock and metadata with the same A/B fallback logic
    /// as [`Pager::open`](crate::Pager::open).  Returns
    /// [`MappedPageError::InvalidPageSize`] if the on-disk page size does not
    /// match `PAGE_SIZE`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, MappedPageError> {
        const {
            assert!(
                PAGE_SIZE.is_power_of_two(),
                "PAGE_SIZE must be a power of two"
            )
        };
        const { assert!(PAGE_SIZE >= 1024, "PAGE_SIZE must be at least 1024") };

        let file = OpenOptions::new().read(true).open(path)?;
        let mmap = unsafe { Mmap::map(&file) }?;

        if mmap.len() < 20 {
            return Err(MappedPageError::CorruptSuperblock);
        }

        let sb = Superblock::from_bytes(&mmap[0..20])
            .filter(|sb| sb.is_valid())
            .ok_or(MappedPageError::CorruptSuperblock)?;

        if (1usize).checked_shl(sb.page_size_log2) != Some(PAGE_SIZE) {
            return Err(MappedPageError::InvalidPageSize);
        }

        if mmap.len() < 4 * PAGE_SIZE {
            return Err(MappedPageError::CorruptSuperblock);
        }

        let active = sb.active_meta;
        let alt = active.other();

        let active_opt: Option<MetaPage> = {
            let off = active.page_id() as usize * PAGE_SIZE;
            let page = &mmap[off..off + PAGE_SIZE];
            MetaPage::from_bytes(page).filter(|_| MetaPage::page_checksum(page) == sb.meta_checksum)
        };

        let meta = if let Some(m) = active_opt {
            m
        } else {
            let off = alt.page_id() as usize * PAGE_SIZE;
            let page = &mmap[off..off + PAGE_SIZE];
            MetaPage::from_bytes(page).ok_or(MappedPageError::CorruptMetadata)?
        };

        let mut dir_blocks = read_dir_blocks(&mmap[0..PAGE_SIZE])
            .map_err(|_| MappedPageError::CorruptDirectoryIndex)?;

        let mut dir_pages = Vec::with_capacity(dir_blocks.len());
        for block in dir_blocks.iter_mut() {
            let active_phys = match block.active {
                MetaSelector::A => block.page_a,
                MetaSelector::B => block.page_b,
            };
            let inactive_phys = match block.active {
                MetaSelector::A => block.page_b,
                MetaSelector::B => block.page_a,
            };

            let try_parse = |phys: u64| -> Option<DirPage> {
                let off = phys as usize * PAGE_SIZE;
                let end = off + PAGE_SIZE;
                if end > mmap.len() {
                    return None;
                }
                DirPage::from_bytes(&mmap[off..end])
            };

            if let Some(dp) = try_parse(active_phys) {
                dir_pages.push(dp);
            } else if let Some(dp) = try_parse(inactive_phys) {
                block.active = block.active.other();
                dir_pages.push(dp);
            } else {
                return Err(MappedPageError::CorruptProtectedDirectory);
            }
        }

        Ok(ReadOnlyPager {
            _file: file,
            mmap,
            meta,
            dir_blocks,
            dir_pages,
        })
    }

    // ── Metadata accessors ────────────────────────────────────────────────────

    /// The page size this file was created with, in bytes.
    pub fn page_size(&self) -> usize {
        PAGE_SIZE
    }

    /// Total number of pages in the file, including reserved pages 0–2.
    pub fn page_count(&self) -> u64 {
        self.meta.total_pages
    }

    /// Number of pages currently available for allocation (as of the last commit
    /// visible when this pager was opened).
    pub fn free_page_count(&self) -> u64 {
        self.meta.free_count
    }

    // ── Page access ───────────────────────────────────────────────────────────

    /// Immutably borrow the contents of a regular data page.
    ///
    /// Returns an error if `id` refers to a reserved page (0–2) or is out of range.
    pub fn get_page(&self, id: PageId<PAGE_SIZE>) -> Result<&MappedPage, MappedPageError> {
        if id.0 < FIRST_DATA_PAGE {
            return Err(MappedPageError::ReservedPage);
        }
        if id.0 >= self.meta.total_pages {
            return Err(MappedPageError::OutOfBounds);
        }
        let off = id.0 as usize * PAGE_SIZE;
        Ok(unsafe { MappedPage::from_slice(&self.mmap[off..off + PAGE_SIZE]) })
    }

    /// Immutably borrow the active copy of a protected page.
    ///
    /// Returns an error if `id` is out of range or not currently allocated.
    pub fn get_protected_page(
        &self,
        id: ProtectedPageId<PAGE_SIZE>,
    ) -> Result<&MappedPage, MappedPageError> {
        let epp = dir_entries_per_page(PAGE_SIZE);
        let block_idx = id.0 as usize / epp;
        let slot = id.0 as usize % epp;
        let entry = self.dir_entry(block_idx, slot)?;
        let phys = if entry.active_slot == 0 {
            entry.page_a
        } else {
            entry.page_b
        };
        let off = phys as usize * PAGE_SIZE;
        Ok(unsafe { MappedPage::from_slice(&self.mmap[off..off + PAGE_SIZE]) })
    }

    // ── Page iteration ────────────────────────────────────────────────────────

    /// Return an iterator over all currently allocated regular data pages.
    ///
    /// Internal protected-page resources and reserved pages 0–2 are excluded.
    /// The iterator borrows the pager immutably for its lifetime.
    pub fn iter_allocated_pages(&self) -> ReadOnlyAllocatedPageIter<'_, PAGE_SIZE> {
        ReadOnlyAllocatedPageIter {
            pager: self,
            current: FIRST_DATA_PAGE,
        }
    }

    /// Return an iterator over all currently allocated protected pages.
    ///
    /// Regular data pages and internal directory-block pages are excluded.
    /// The iterator borrows the pager immutably for its lifetime.
    pub fn iter_allocated_protected_pages(
        &self,
    ) -> ReadOnlyAllocatedProtectedPageIter<'_, PAGE_SIZE> {
        ReadOnlyAllocatedProtectedPageIter {
            pager: self,
            block_idx: 0,
            slot_idx: 0,
        }
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    fn is_internal_page(&self, id: u64) -> bool {
        for block in &self.dir_blocks {
            if block.page_a == id || block.page_b == id {
                return true;
            }
        }
        for dir_page in &self.dir_pages {
            for entry in &dir_page.entries {
                if entry.in_use && (entry.page_a == id || entry.page_b == id) {
                    return true;
                }
            }
        }
        false
    }

    fn dir_entry(&self, block_idx: usize, slot: usize) -> Result<&DirEntry, MappedPageError> {
        self.dir_pages
            .get(block_idx)
            .and_then(|dp| dp.entries.get(slot))
            .filter(|e| e.in_use)
            .ok_or(MappedPageError::OutOfBounds)
    }
}

// ── ReadOnlyAllocatedPageIter ─────────────────────────────────────────────────

/// An iterator over all currently allocated data pages in a [`ReadOnlyPager`].
///
/// Yielded by [`ReadOnlyPager::iter_allocated_pages`].  Reserved pages 0–2 and
/// internal protected-page resources are never included.
pub struct ReadOnlyAllocatedPageIter<'a, const PAGE_SIZE: usize> {
    pager: &'a ReadOnlyPager<PAGE_SIZE>,
    current: u64,
}

impl<'a, const PAGE_SIZE: usize> Iterator for ReadOnlyAllocatedPageIter<'a, PAGE_SIZE> {
    type Item = PageId<PAGE_SIZE>;

    fn next(&mut self) -> Option<Self::Item> {
        while self.current < self.pager.meta.total_pages {
            let id = self.current;
            self.current += 1;
            let byte_idx = (id / 8) as usize;
            let bit = (id % 8) as u8;
            if self.pager.meta.bitmap[byte_idx] & (1 << bit) != 0
                && !self.pager.is_internal_page(id)
            {
                return Some(PageId(id));
            }
        }
        None
    }
}

// ── ReadOnlyAllocatedProtectedPageIter ───────────────────────────────────────

/// An iterator over all currently allocated protected pages in a [`ReadOnlyPager`].
///
/// Yielded by [`ReadOnlyPager::iter_allocated_protected_pages`].  Regular data
/// pages and internal directory-block pages are never included.
pub struct ReadOnlyAllocatedProtectedPageIter<'a, const PAGE_SIZE: usize> {
    pager: &'a ReadOnlyPager<PAGE_SIZE>,
    block_idx: usize,
    slot_idx: usize,
}

impl<'a, const PAGE_SIZE: usize> Iterator for ReadOnlyAllocatedProtectedPageIter<'a, PAGE_SIZE> {
    type Item = ProtectedPageId<PAGE_SIZE>;

    fn next(&mut self) -> Option<Self::Item> {
        let epp = dir_entries_per_page(PAGE_SIZE);
        loop {
            let dir_page = self.pager.dir_pages.get(self.block_idx)?;
            if self.slot_idx >= dir_page.entries.len() {
                self.block_idx += 1;
                self.slot_idx = 0;
                continue;
            }
            let entry = &dir_page.entries[self.slot_idx];
            let id = ProtectedPageId((self.block_idx * epp + self.slot_idx) as u64);
            self.slot_idx += 1;
            if entry.in_use {
                return Some(id);
            }
        }
    }
}
