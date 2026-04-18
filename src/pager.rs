//! The `Pager`: file + mmap management, allocation, and crash-safe commits.

use std::fs::{File, OpenOptions};
use std::path::Path;

use memmap2::MmapMut;

use crate::error::MappedPageError;
use crate::meta::{MetaPage, MetaSelector, Superblock, FIRST_DATA_PAGE, MAGIC, MIN_PAGE_SIZE_LOG2};
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
/// obtained before the grow is **invalid** after it returns.
pub struct Pager {
    file: File,
    /// `None` only transiently during remap inside `grow`.
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
        // validate: page_size_log2 >= MIN_PAGE_SIZE_LOG2
        todo!()
    }

    /// Open an existing pager file, validating and recovering metadata.
    ///
    /// On open the superblock is read; if either metadata page fails its
    /// checksum the other is tried.  Both failing is an error.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, MappedPageError> {
        todo!()
    }

    // ── Allocation ────────────────────────────────────────────────────────────

    /// Allocate a fresh page.  Grows the file if needed.
    ///
    /// Pages 0-2 are never returned.
    pub fn alloc(&mut self) -> Result<PageId, MappedPageError> {
        todo!()
    }

    /// Mark `id` as free so it can be returned by a future `alloc`.
    ///
    /// Returns an error if `id` is out of range, reserved (0-2), or already free.
    pub fn free(&mut self, id: PageId) -> Result<(), MappedPageError> {
        todo!()
    }

    // ── Metadata accessors ────────────────────────────────────────────────────

    /// The page size this pager was created with, in bytes.
    pub fn page_size(&self) -> usize {
        self.page_size
    }

    /// Total number of pages in the file, including reserved pages 0-2.
    pub fn page_count(&self) -> u64 {
        self.meta.total_pages
    }

    // ── Page access (called by PageId) ────────────────────────────────────────

    pub(crate) fn get_page(&self, id: PageId) -> Result<&MappedPage, MappedPageError> {
        todo!()
    }

    pub(crate) fn get_page_mut(&mut self, id: PageId) -> Result<&mut MappedPage, MappedPageError> {
        todo!()
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    fn mmap(&self) -> &MmapMut {
        self.mmap.as_ref().expect("mmap is always Some outside of grow")
    }

    fn mmap_mut(&mut self) -> &mut MmapMut {
        self.mmap.as_mut().expect("mmap is always Some outside of grow")
    }

    fn page_slice(&self, id: u64) -> &[u8] {
        let off = id as usize * self.page_size;
        &self.mmap()[off..off + self.page_size]
    }

    fn page_slice_mut(&mut self, id: u64) -> &mut [u8] {
        let off = id as usize * self.page_size;
        let ps = self.page_size;
        &mut self.mmap_mut()[off..off + ps]
    }

    /// Double-buffered commit: write metadata to the inactive page, msync,
    /// then flip the superblock pointer and msync the superblock.
    fn commit(&mut self) -> Result<(), MappedPageError> {
        todo!()
    }

    /// Extend the file to the next power-of-two page count, remap, and commit.
    fn grow(&mut self) -> Result<(), MappedPageError> {
        todo!()
    }
}
