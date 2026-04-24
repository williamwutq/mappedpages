//! The `Pager`: file + mmap management, allocation, and crash-safe commits.

use std::fs::{File, OpenOptions};
use std::path::Path;

use memmap2::MmapMut;

use crate::error::MappedPageError;
use crate::meta::{
    DirBlockRef, DirEntry, DirPage, FIRST_DATA_PAGE, MAGIC, MetaPage, MetaSelector, Superblock,
    dir_entries_per_page, max_dir_blocks, read_dir_blocks, write_dir_blocks,
};
use crate::page::{MappedPage, PageId};
use crate::protected::{ProtectedPageId, ProtectedPageWriter};

/// Manages a memory-mapped, fixed-size-page file.
///
/// The const generic `PAGE_SIZE` is the page size in bytes.  It must be a
/// power of two and at least 1024.  Using a mismatched `PAGE_SIZE` when
/// opening an existing file returns [`MappedPageError::InvalidPageSize`].
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
pub struct Pager<const PAGE_SIZE: usize> {
    file: File,
    /// `None` only after a failed remap; all operations return `Unavailable`.
    pub(crate) mmap: Option<MmapMut>,
    active_meta: MetaSelector,
    /// In-memory working copy of the active metadata.
    meta: MetaPage,
    /// Directory block references stored in page 0's extended section.
    /// Empty when no protected pages have ever been allocated.
    dir_blocks: Vec<DirBlockRef>,
    /// In-memory view of the active directory page for each block, parallel to `dir_blocks`.
    dir_pages: Vec<DirPage>,
}

impl<const PAGE_SIZE: usize> Pager<PAGE_SIZE> {
    // ── Construction ──────────────────────────────────────────────────────────

    /// Create a new pager backed by `path` with page size `PAGE_SIZE` bytes.
    ///
    /// `PAGE_SIZE` must be a power of two and at least 1024; violating either
    /// constraint is a **compile error**.  The file must not already exist.
    pub fn create(path: impl AsRef<Path>) -> Result<Self, MappedPageError> {
        const {
            assert!(
                PAGE_SIZE.is_power_of_two(),
                "PAGE_SIZE must be a power of two"
            )
        };
        const { assert!(PAGE_SIZE >= 1024, "PAGE_SIZE must be at least 1024") };

        let page_size_log2 = PAGE_SIZE.trailing_zeros();
        let initial_pages = 4u64;

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;

        file.set_len(initial_pages * PAGE_SIZE as u64)?;

        let mut mmap = unsafe { MmapMut::map_mut(&file) }?;

        let meta = MetaPage::new_for_capacity(initial_pages);

        // Serialize metadata; write to both page 1 (A) and page 2 (B).
        let mut meta_buf = vec![0u8; PAGE_SIZE];
        meta.write_to(&mut meta_buf);
        mmap[PAGE_SIZE..2 * PAGE_SIZE].copy_from_slice(&meta_buf);
        mmap[2 * PAGE_SIZE..3 * PAGE_SIZE].copy_from_slice(&meta_buf);

        // Write full page 0: superblock + empty dir section.
        let meta_checksum = MetaPage::page_checksum(&meta_buf);
        let sb = Superblock {
            magic: MAGIC,
            page_size_log2,
            active_meta: MetaSelector::A,
            meta_checksum,
        };
        let mut page0_buf = vec![0u8; PAGE_SIZE];
        sb.write_to(&mut page0_buf[0..20]);
        write_dir_blocks(&[], &mut page0_buf);
        mmap[0..PAGE_SIZE].copy_from_slice(&page0_buf);

        mmap.flush()?;

        Ok(Pager {
            file,
            mmap: Some(mmap),
            active_meta: MetaSelector::A,
            meta,
            dir_blocks: vec![],
            dir_pages: vec![],
        })
    }

    /// Open an existing pager file, validating and recovering metadata.
    ///
    /// Returns [`MappedPageError::InvalidPageSize`] if the on-disk page size
    /// does not match `PAGE_SIZE`.
    ///
    /// The superblock is read first; from it we learn the page size and which
    /// metadata page is active.  The active page is then validated against both
    /// its own embedded checksum and the superblock's `meta_checksum`.  If it
    /// fails, the alternate is tried (internal checksum only).  Both failing is
    /// an error.
    ///
    /// Protected-page directory blocks are loaded from page 0's extended section
    /// with the same A/B fallback logic.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, MappedPageError> {
        const {
            assert!(
                PAGE_SIZE.is_power_of_two(),
                "PAGE_SIZE must be a power of two"
            )
        };
        const { assert!(PAGE_SIZE >= 1024, "PAGE_SIZE must be at least 1024") };

        let file = OpenOptions::new().read(true).write(true).open(path)?;

        let mmap = unsafe { MmapMut::map_mut(&file) }?;

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

        // Try the superblock-designated active page: internal checksum + superblock checksum.
        let active_opt: Option<MetaPage> = {
            let off = active.page_id() as usize * PAGE_SIZE;
            let page = &mmap[off..off + PAGE_SIZE];
            MetaPage::from_bytes(page).filter(|_| MetaPage::page_checksum(page) == sb.meta_checksum)
        };

        // Fall back to the alternate page: internal checksum only.
        let (meta, active_meta) = if let Some(m) = active_opt {
            (m, active)
        } else {
            let off = alt.page_id() as usize * PAGE_SIZE;
            let page = &mmap[off..off + PAGE_SIZE];
            let m = MetaPage::from_bytes(page).ok_or(MappedPageError::CorruptMetadata)?;
            (m, alt)
        };

        // Load directory block references from page 0's extended section.
        let mut dir_blocks = read_dir_blocks(&mmap[0..PAGE_SIZE])
            .map_err(|_| MappedPageError::CorruptDirectoryIndex)?;

        // Load the active directory page for each block, falling back to the alternate.
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
                // Active page was corrupt; recover from alternate and correct the selector.
                block.active = block.active.other();
                dir_pages.push(dp);
            } else {
                return Err(MappedPageError::CorruptProtectedDirectory);
            }
        }

        Ok(Pager {
            file,
            mmap: Some(mmap),
            active_meta,
            meta,
            dir_blocks,
            dir_pages,
        })
    }

    // ── Allocation ────────────────────────────────────────────────────────────

    /// Allocate a fresh page.  Grows the file if no free pages remain.
    ///
    /// Pages 0–2 are never returned.
    pub fn alloc(&mut self) -> Result<PageId<PAGE_SIZE>, MappedPageError> {
        let id = self.alloc_one_raw()?;
        self.commit()?;
        Ok(PageId(id))
    }

    /// Mark `id` as free so it can be returned by a future `alloc`.
    ///
    /// Returns an error if `id` is reserved (0–2), out of range, or already free.
    pub fn free(&mut self, id: PageId<PAGE_SIZE>) -> Result<(), MappedPageError> {
        if id.0 < FIRST_DATA_PAGE {
            return Err(MappedPageError::ReservedPage);
        }
        if !self.meta.free_page(id.0) {
            return Err(if id.0 >= self.meta.total_pages {
                MappedPageError::OutOfBounds
            } else {
                MappedPageError::DoubleFree
            });
        }
        self.commit()
    }

    // ── Async I/O Support ────────────────────────────────────────────────────

    /// Asynchronously allocate a page.
    ///
    /// This is the async version of [`alloc`]. It performs the same operation
    /// but allows the caller to await other tasks while I/O operations complete.
    ///
    /// Requires the "async" feature to be enabled.
    #[cfg(feature = "async")]
    pub async fn alloc_async(&mut self) -> Result<PageId<PAGE_SIZE>, MappedPageError> {
        let id = self.alloc_one_raw()?;
        self.commit_async().await?;
        Ok(PageId(id))
    }

    /// Asynchronously mark `id` as free.
    ///
    /// This is the async version of [`free`]. It performs the same operation
    /// but allows the caller to await other tasks while I/O operations complete.
    ///
    /// Requires the "async" feature to be enabled.
    #[cfg(feature = "async")]
    pub async fn free_async(&mut self, id: PageId<PAGE_SIZE>) -> Result<(), MappedPageError> {
        if id.0 < FIRST_DATA_PAGE {
            return Err(MappedPageError::ReservedPage);
        }
        if !self.meta.free_page(id.0) {
            return Err(if id.0 >= self.meta.total_pages {
                MappedPageError::OutOfBounds
            } else {
                MappedPageError::DoubleFree
            });
        }
        self.commit_async().await
    }

    /// Asynchronously commit metadata changes to disk.
    ///
    /// This is the async version of the internal `commit` method.
    /// Note: Currently blocks the async runtime thread due to mmap flush operations.
    /// In future versions, this may be made truly async.
    #[cfg(feature = "async")]
    async fn commit_async(&mut self) -> Result<(), MappedPageError> {
        self.commit()
    }

    // ── Protected-page allocation ─────────────────────────────────────────────

    /// Allocate a protected (crash-consistent copy-on-write) page.
    ///
    /// On the first call, two physical pages are reserved as the A/B directory
    /// block; their locations are recorded in page 0.  If all existing directory
    /// blocks are full, another pair is allocated.  Two additional physical pages
    /// are always allocated as the backing copies for the new protected page.
    pub fn alloc_protected(&mut self) -> Result<ProtectedPageId<PAGE_SIZE>, MappedPageError> {
        let epp = dir_entries_per_page(PAGE_SIZE);

        // Try to claim a free slot in an existing directory block.
        for block_idx in 0..self.dir_pages.len() {
            if let Some(slot) = self.dir_pages[block_idx]
                .entries
                .iter()
                .position(|e| !e.in_use)
            {
                // Allocate two backing pages and commit the normal metadata.
                let pa = self.alloc_one_raw()?;
                let pb = self.alloc_one_raw()?;
                self.commit()?;

                let checksum = self.page_checksum_at(pa);
                self.dir_pages[block_idx].entries[slot] = DirEntry {
                    in_use: true,
                    page_a: pa,
                    page_b: pb,
                    active_slot: 0,
                    generation: 0,
                    checksum,
                };
                self.commit_dir_block(block_idx)?;

                return Ok(ProtectedPageId((block_idx * epp + slot) as u64));
            }
        }

        // No free slot: need a new directory block pair.
        if self.dir_blocks.len() >= max_dir_blocks(PAGE_SIZE) {
            return Err(MappedPageError::DirectoryFull);
        }

        // Allocate 4 pages at once (2 for dir A/B, 2 for data A/B) in a single commit.
        let dir_pa = self.alloc_one_raw()?;
        let dir_pb = self.alloc_one_raw()?;
        let data_pa = self.alloc_one_raw()?;
        let data_pb = self.alloc_one_raw()?;
        self.commit()?;

        let block_idx = self.dir_blocks.len();
        self.dir_blocks.push(DirBlockRef {
            page_a: dir_pa,
            page_b: dir_pb,
            active: MetaSelector::A,
        });
        let mut new_dir_page = DirPage::new_empty(PAGE_SIZE);
        let checksum = self.page_checksum_at(data_pa);
        new_dir_page.entries[0] = DirEntry {
            in_use: true,
            page_a: data_pa,
            page_b: data_pb,
            active_slot: 0,
            generation: 0,
            checksum,
        };
        self.dir_pages.push(new_dir_page);

        // Write the new directory page (to inactive=B) and update page 0.
        self.commit_dir_block(block_idx)?;

        Ok(ProtectedPageId((block_idx * epp) as u64))
    }

    /// Free a protected page, releasing both its backing physical pages.
    ///
    /// Returns `DoubleFree` if the slot is already free, `OutOfBounds` if the
    /// id is out of range.
    pub fn free_protected(
        &mut self,
        id: ProtectedPageId<PAGE_SIZE>,
    ) -> Result<(), MappedPageError> {
        let epp = dir_entries_per_page(PAGE_SIZE);
        let block_idx = id.0 as usize / epp;
        let slot = id.0 as usize % epp;

        let (pa, pb) = {
            let entry = self.dir_entry_mut(block_idx, slot)?;
            if !entry.in_use {
                return Err(MappedPageError::DoubleFree);
            }
            let pa = entry.page_a;
            let pb = entry.page_b;
            entry.in_use = false;
            (pa, pb)
        };

        // Mark the slot as free in the directory first (crash-safe order).
        self.commit_dir_block(block_idx)?;

        // Then release the backing pages in normal metadata.
        self.meta.free_page(pa);
        self.meta.free_page(pb);
        self.commit()
    }

    /// Asynchronously allocate a protected (crash-consistent copy-on-write) page.
    ///
    /// This is the async version of [`alloc_protected`].
    #[cfg(feature = "async")]
    pub async fn alloc_protected_async(
        &mut self,
    ) -> Result<ProtectedPageId<PAGE_SIZE>, MappedPageError> {
        let epp = dir_entries_per_page(PAGE_SIZE);

        // Try to claim a free slot in an existing directory block.
        for block_idx in 0..self.dir_pages.len() {
            if let Some(slot) = self.dir_pages[block_idx]
                .entries
                .iter()
                .position(|e| !e.in_use)
            {
                // Allocate two backing pages and commit the normal metadata.
                let pa = self.alloc_one_raw()?;
                let pb = self.alloc_one_raw()?;
                self.commit_async().await?;

                let checksum = self.page_checksum_at(pa);
                self.dir_pages[block_idx].entries[slot] = DirEntry {
                    in_use: true,
                    page_a: pa,
                    page_b: pb,
                    active_slot: 0,
                    generation: 0,
                    checksum,
                };
                self.commit_dir_block_async(block_idx).await?;

                return Ok(ProtectedPageId((block_idx * epp + slot) as u64));
            }
        }

        // No free slot: need a new directory block pair.
        if self.dir_blocks.len() >= max_dir_blocks(PAGE_SIZE) {
            return Err(MappedPageError::DirectoryFull);
        }

        // Allocate 4 pages at once (2 for dir A/B, 2 for data A/B) in a single commit.
        let dir_pa = self.alloc_one_raw()?;
        let dir_pb = self.alloc_one_raw()?;
        let data_pa = self.alloc_one_raw()?;
        let data_pb = self.alloc_one_raw()?;
        self.commit_async().await?;

        let block_idx = self.dir_blocks.len();
        self.dir_blocks.push(DirBlockRef {
            page_a: dir_pa,
            page_b: dir_pb,
            active: MetaSelector::A,
        });
        let mut new_dir_page = DirPage::new_empty(PAGE_SIZE);
        let checksum = self.page_checksum_at(data_pa);
        new_dir_page.entries[0] = DirEntry {
            in_use: true,
            page_a: data_pa,
            page_b: data_pb,
            active_slot: 0,
            generation: 0,
            checksum,
        };
        self.dir_pages.push(new_dir_page);

        // Write the new directory page (to inactive=B) and update page 0.
        self.commit_dir_block_async(block_idx).await?;

        Ok(ProtectedPageId((block_idx * epp) as u64))
    }

    /// Asynchronously free a protected page.
    ///
    /// This is the async version of [`free_protected`].
    #[cfg(feature = "async")]
    pub async fn free_protected_async(
        &mut self,
        id: ProtectedPageId<PAGE_SIZE>,
    ) -> Result<(), MappedPageError> {
        let epp = dir_entries_per_page(PAGE_SIZE);
        let block_idx = id.0 as usize / epp;
        let slot = id.0 as usize % epp;

        let (pa, pb) = {
            let entry = self.dir_entry_mut(block_idx, slot)?;
            if !entry.in_use {
                return Err(MappedPageError::DoubleFree);
            }
            let pa = entry.page_a;
            let pb = entry.page_b;
            entry.in_use = false;
            (pa, pb)
        };

        // Mark the slot as free in the directory first (crash-safe order).
        self.commit_dir_block_async(block_idx).await?;

        // Then release the backing pages in normal metadata.
        self.meta.free_page(pa);
        self.meta.free_page(pb);
        self.commit_async().await
    }

    /// Asynchronously commit directory block changes.
    /// Note: Currently blocks the async runtime thread.
    #[cfg(feature = "async")]
    async fn commit_dir_block_async(&mut self, block_idx: usize) -> Result<(), MappedPageError> {
        self.commit_dir_block(block_idx)
    }

    // ── Metadata accessors ────────────────────────────────────────────────────

    /// The page size this pager was created with, in bytes.
    pub fn page_size(&self) -> usize {
        PAGE_SIZE
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

    /// Returns (page_a, page_b, active_selector) for one directory block (test-only).
    #[cfg(test)]
    pub(crate) fn dir_block_pages(&self, block_idx: usize) -> (u64, u64, MetaSelector) {
        let b = &self.dir_blocks[block_idx];
        (b.page_a, b.page_b, b.active)
    }

    /// Which slot (0 = page_a, 1 = page_b) is the active copy of a protected page (test-only).
    #[cfg(test)]
    pub(crate) fn protected_active_slot(&self, id: ProtectedPageId<PAGE_SIZE>) -> u8 {
        let epp = dir_entries_per_page(PAGE_SIZE);
        let block_idx = id.0 as usize / epp;
        let slot = id.0 as usize % epp;
        self.dir_pages[block_idx].entries[slot].active_slot
    }

    /// Physical page numbers (page_a, page_b) backing a protected page (test-only).
    #[cfg(test)]
    pub(crate) fn protected_backing_pages(&self, id: ProtectedPageId<PAGE_SIZE>) -> (u64, u64) {
        let epp = dir_entries_per_page(PAGE_SIZE);
        let block_idx = id.0 as usize / epp;
        let slot = id.0 as usize % epp;
        let e = &self.dir_pages[block_idx].entries[slot];
        (e.page_a, e.page_b)
    }

    // ── Page access (called by PageId / ProtectedPageId) ──────────────────────

    pub(crate) fn get_page(&self, id: PageId<PAGE_SIZE>) -> Result<&MappedPage, MappedPageError> {
        if id.0 < FIRST_DATA_PAGE {
            return Err(MappedPageError::ReservedPage);
        }
        if id.0 >= self.meta.total_pages {
            return Err(MappedPageError::OutOfBounds);
        }
        let off = id.0 as usize * PAGE_SIZE;
        let slice = &self.mmap()?[off..off + PAGE_SIZE];
        Ok(unsafe { MappedPage::from_slice(slice) })
    }

    pub(crate) fn get_page_mut(
        &mut self,
        id: PageId<PAGE_SIZE>,
    ) -> Result<&mut MappedPage, MappedPageError> {
        if id.0 < FIRST_DATA_PAGE {
            return Err(MappedPageError::ReservedPage);
        }
        if id.0 >= self.meta.total_pages {
            return Err(MappedPageError::OutOfBounds);
        }
        let off = id.0 as usize * PAGE_SIZE;
        let slice = &mut self.mmap_mut()?[off..off + PAGE_SIZE];
        Ok(unsafe { MappedPage::from_slice_mut(slice) })
    }

    pub(crate) fn get_protected_page(
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
        Ok(unsafe { MappedPage::from_slice(&self.mmap()?[off..off + PAGE_SIZE]) })
    }

    pub(crate) fn get_protected_page_mut(
        &mut self,
        id: ProtectedPageId<PAGE_SIZE>,
    ) -> Result<ProtectedPageWriter<'_, PAGE_SIZE>, MappedPageError> {
        let epp = dir_entries_per_page(PAGE_SIZE);
        let block_idx = id.0 as usize / epp;
        let slot = id.0 as usize % epp;
        let (inactive_phys, inactive_slot) = {
            let entry = self.dir_entry(block_idx, slot)?;
            let isl = 1 - entry.active_slot;
            let ip = if isl == 0 { entry.page_a } else { entry.page_b };
            (ip, isl)
        };
        self.mmap()?; // verify mmap is available before handing out the writer
        Ok(ProtectedPageWriter {
            pager: self,
            id,
            inactive_phys_page: inactive_phys,
            inactive_slot,
        })
    }

    /// Called by `ProtectedPageWriter::commit` to finalise a protected-page write.
    pub(crate) fn commit_protected_write(
        &mut self,
        id: ProtectedPageId<PAGE_SIZE>,
        inactive_phys: u64,
        inactive_slot: u8,
    ) -> Result<(), MappedPageError> {
        let epp = dir_entries_per_page(PAGE_SIZE);
        let block_idx = id.0 as usize / epp;
        let slot = id.0 as usize % epp;

        // Step 1: flush the inactive physical page.
        let inactive_off = inactive_phys as usize * PAGE_SIZE;
        self.mmap()?.flush_range(inactive_off, PAGE_SIZE)?;

        // Step 2: compute checksum of the newly written page.
        let new_checksum = {
            let mmap = self.mmap()?;
            crc32fast::hash(&mmap[inactive_off..inactive_off + PAGE_SIZE])
        };

        // Step 3: update the in-memory directory entry.
        {
            let entry = self.dir_entry_mut(block_idx, slot)?;
            entry.active_slot = inactive_slot;
            entry.generation += 1;
            entry.checksum = new_checksum;
        }

        // Step 4: commit the directory block (flip A/B, update page 0).
        self.commit_dir_block(block_idx)
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    fn mmap(&self) -> Result<&MmapMut, MappedPageError> {
        self.mmap.as_ref().ok_or(MappedPageError::Unavailable)
    }

    fn mmap_mut(&mut self) -> Result<&mut MmapMut, MappedPageError> {
        self.mmap.as_mut().ok_or(MappedPageError::Unavailable)
    }

    /// CRC32 of the physical page at `phys_page_id`.
    fn page_checksum_at(&self, phys_page_id: u64) -> u32 {
        let off = phys_page_id as usize * PAGE_SIZE;
        match self.mmap.as_ref() {
            Some(m) => crc32fast::hash(&m[off..off + PAGE_SIZE]),
            None => 0,
        }
    }

    /// Borrow a directory entry (immutable).
    fn dir_entry(&self, block_idx: usize, slot: usize) -> Result<&DirEntry, MappedPageError> {
        self.dir_pages
            .get(block_idx)
            .and_then(|dp| dp.entries.get(slot))
            .filter(|e| e.in_use)
            .ok_or(MappedPageError::OutOfBounds)
    }

    /// Borrow a directory entry (mutable), checking bounds but not `in_use`.
    fn dir_entry_mut(
        &mut self,
        block_idx: usize,
        slot: usize,
    ) -> Result<&mut DirEntry, MappedPageError> {
        self.dir_pages
            .get_mut(block_idx)
            .and_then(|dp| dp.entries.get_mut(slot))
            .ok_or(MappedPageError::OutOfBounds)
    }

    /// Allocate one physical page from the normal allocator without committing.
    fn alloc_one_raw(&mut self) -> Result<u64, MappedPageError> {
        if let Some(id) = self.meta.alloc_page() {
            return Ok(id);
        }
        self.grow()?;
        Ok(self.meta.alloc_page().expect("grow always adds free pages"))
    }

    /// Double-buffered commit for normal metadata (allocation bitmap):
    /// 1. Serialize `self.meta` into the *inactive* metadata page and msync it.
    /// 2. Rewrite page 0 (superblock + dir blocks) pointing to the inactive page and msync it.
    /// 3. Flip `self.active_meta`.
    fn commit(&mut self) -> Result<(), MappedPageError> {
        let inactive = self.active_meta.other();
        let inactive_off = inactive.page_id() as usize * PAGE_SIZE;

        self.meta.generation += 1;

        let mut meta_buf = vec![0u8; PAGE_SIZE];
        self.meta.write_to(&mut meta_buf);
        let meta_checksum = MetaPage::page_checksum(&meta_buf);

        // Step 1: write metadata to inactive page, then msync.
        self.mmap_mut()?[inactive_off..inactive_off + PAGE_SIZE].copy_from_slice(&meta_buf);
        self.mmap()?.flush_range(inactive_off, PAGE_SIZE)?;

        // Step 2: write full page 0 (superblock + dir block array), then msync.
        let mut page0_buf = vec![0u8; PAGE_SIZE];
        let sb = Superblock {
            magic: MAGIC,
            page_size_log2: PAGE_SIZE.trailing_zeros(),
            active_meta: inactive,
            meta_checksum,
        };
        sb.write_to(&mut page0_buf[0..20]);
        write_dir_blocks(&self.dir_blocks, &mut page0_buf);
        self.mmap_mut()?[0..PAGE_SIZE].copy_from_slice(&page0_buf);
        self.mmap()?.flush_range(0, PAGE_SIZE)?;

        // Step 3: commit is durable; update in-memory pointer.
        self.active_meta = inactive;
        Ok(())
    }

    /// Crash-safe commit for one directory block:
    /// 1. Serialize the in-memory directory page to the *inactive* physical dir page and msync.
    /// 2. Flip the active selector for this block.
    /// 3. Rewrite page 0 to record the new active selector and msync.
    fn commit_dir_block(&mut self, block_idx: usize) -> Result<(), MappedPageError> {
        // Serialize current in-memory dir page to a temp buffer.
        let mut dir_buf = vec![0u8; PAGE_SIZE];
        self.dir_pages[block_idx].write_to(&mut dir_buf);

        // Identify the inactive physical dir page.
        let block = self.dir_blocks[block_idx];
        let inactive_sel = block.active.other();
        let inactive_phys = match inactive_sel {
            MetaSelector::A => block.page_a,
            MetaSelector::B => block.page_b,
        };
        let inactive_off = inactive_phys as usize * PAGE_SIZE;

        // Step 1: write to inactive dir page and flush.
        self.mmap_mut()?[inactive_off..inactive_off + PAGE_SIZE].copy_from_slice(&dir_buf);
        self.mmap()?.flush_range(inactive_off, PAGE_SIZE)?;

        // Step 2: flip the in-memory active selector.
        self.dir_blocks[block_idx].active = inactive_sel;

        // Step 3: compute current meta checksum from the on-disk active meta page,
        // then write the full page 0 with updated dir blocks and flush.
        let active_meta = self.active_meta;
        let meta_checksum = {
            let meta_off = active_meta.page_id() as usize * PAGE_SIZE;
            let mmap = self.mmap()?;
            MetaPage::page_checksum(&mmap[meta_off..meta_off + PAGE_SIZE])
        };

        let mut page0_buf = vec![0u8; PAGE_SIZE];
        let sb = Superblock {
            magic: MAGIC,
            page_size_log2: PAGE_SIZE.trailing_zeros(),
            active_meta,
            meta_checksum,
        };
        sb.write_to(&mut page0_buf[0..20]);
        write_dir_blocks(&self.dir_blocks, &mut page0_buf);
        self.mmap_mut()?[0..PAGE_SIZE].copy_from_slice(&page0_buf);
        self.mmap()?.flush_range(0, PAGE_SIZE)?;

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
        let old_file_size = self.meta.total_pages * PAGE_SIZE as u64;
        let new_file_size = new_total * PAGE_SIZE as u64;

        // Drop the mmap before resizing; required on all platforms.
        drop(self.mmap.take());

        if let Err(e) = self.file.set_len(new_file_size) {
            // File size unchanged; restore the mapping at original size.
            self.mmap = Some(unsafe { MmapMut::map_mut(&self.file) }.map_err(MappedPageError::Io)?);
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
