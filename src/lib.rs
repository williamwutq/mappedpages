//! A crash-consistent, memory-mapped, file-backed fixed-size page provider.
//!
//! # Overview
//!
//! This crate provides a `Pager` that manages a file divided into fixed-size pages.
//! Pages are addressed by `PageId` and accessed as `MappedPage` views into the
//! underlying memory map.
//!
//! # File Layout
//!
//! ```text
//! Page 0  — Superblock   (magic, version, page size, active metadata selector + its checksum)
//! Page 1  — Metadata A   (free bitmap, total pages, generation, checksum)
//! Page 2  — Metadata B   (same layout; alternate buffer for crash-safe commits)
//! Page 3+ — Data pages   (user-visible, returned by alloc)
//! ```
//!
//! # Crash-Consistency Guarantee
//!
//! All allocation state changes go through a double-buffered commit:
//!
//! 1. Write updated metadata into the *inactive* metadata page.
//! 2. `msync` that page.
//! 3. Flip the active-metadata pointer in the superblock and update its checksum.
//! 4. `msync` the superblock.
//!
//! The active metadata page is **never** overwritten in place.  Recovery on open
//! validates checksums and falls back to the alternate page if needed.
//!
//! # Page-size safety
//!
//! `Pager`, `PageId`, and `ProtectedPageId` are all generic over a const
//! `PAGE_SIZE: usize`.  A `PageId<1024>` cannot be passed to a `Pager<4096>`
//! — the compiler rejects the mismatch.  `PAGE_SIZE` must be a power of two
//! and at least 1024; violating either constraint is a compile error.
//!
//! # Bulk operations
//!
//! [`Pager::alloc_bulk`] and [`Pager::free_bulk`] allocate or free multiple
//! regular pages in a single crash-safe metadata commit, reducing overhead for
//! workloads that need many pages at once.  [`free_bulk`](Pager::free_bulk)
//! validates all ids atomically before touching the bitmap — a single invalid
//! id causes the whole call to fail without modifying any state.
//!
//! [`Pager::alloc_protected_bulk`] and [`Pager::free_protected_bulk`] do the
//! same for protected pages.  Because each protected-page allocation requires
//! multiple physical pages and directory commits, the bulk variant cannot batch
//! everything into one commit; it does guarantee that on failure all
//! already-allocated protected pages are freed (for alloc) and that all ids are
//! validated before any page is freed (for free).
//!
//! # Page iteration
//!
//! [`Pager::iter_allocated_pages`] returns an [`AllocatedPageIter`] that
//! traverses the allocation bitmap and yields a [`PageId`] for each allocated
//! regular data page.  Internal protected-page resources (directory block pages
//! and backing pages for in-use protected entries) are excluded.  Reserved
//! pages 0–2 are never included.
//!
//! [`Pager::iter_allocated_protected_pages`] returns an
//! [`AllocatedProtectedPageIter`] that traverses the protected-page directory
//! and yields a [`ProtectedPageId`] for each in-use slot.  Regular data pages
//! are never included.
//!
//! Both iterators hold an immutable borrow on the pager, so allocation and
//! deallocation are statically prevented while either is alive.
//!
//! # Sub-page allocation
//!
//! A `mappedpages` file always has a single, fixed page size — it is written
//! to the superblock on [`Pager::create`] and validated on every subsequent
//! [`Pager::open`].  When a workload needs finer granularity than that fixed
//! size, [`SubPageAllocator`] handles the bookkeeping so callers do not have
//! to write their own bitmap-based slab allocators on top of raw pages.
//!
//! [`SubPageAllocator<PARENT_SIZE, SUB_SIZE>`](SubPageAllocator) wraps a
//! [`Pager<PARENT_SIZE>`](Pager), checks out big pages as needed, and divides
//! each one into `PARENT_SIZE / SUB_SIZE` sub-slots (up to 64).  Handles are
//! [`SubPageId<PARENT_SIZE, SUB_SIZE>`](SubPageId), which implement
//! [`PageHandle`] and [`PageAllocator`] just like [`PageId`] and
//! [`ProtectedPageId`].  It also implements [`BulkPageAllocator`], so
//! `alloc_bulk` and `free_bulk` are available for sub-pages with the same
//! all-or-nothing validation semantics.
//!
//! **Sub-allocation state is in-memory only.**  The on-disk file is unchanged —
//! sub-slots live inside the data bytes of ordinary pages, and the free/used
//! bitmasks are never written to disk.  On process restart, callers must
//! reconstruct which sub-slots are in use from their own records before
//! issuing sub-page handles again.
//!
//! # Allocator traits
//!
//! [`PageHandle<A>`](PageHandle) is implemented by [`PageId`],
//! [`ProtectedPageId`], and [`SubPageId`] for their respective allocator
//! types.  [`PageAllocator<H>`](PageAllocator) is the single-page interface;
//! it is implemented by [`Pager`] for both `PageId` and `ProtectedPageId` and
//! by [`SubPageAllocator`] for `SubPageId`.
//!
//! [`BulkPageAllocator<H>`](BulkPageAllocator) is an optional supertrait of
//! [`PageAllocator`] for allocators that can allocate or free multiple handles
//! efficiently, or need all-or-nothing validation semantics across a batch.
//! It is implemented by [`Pager`] for both `PageId` and `ProtectedPageId`, and
//! by [`SubPageAllocator`] for `SubPageId`.  Generic code can require bulk
//! capability with a `where A: BulkPageAllocator<H>` bound.
//!
//! # Reference lifetime and grow safety
//!
//! `&MappedPage` and `&mut MappedPage` are tied to the *borrow* of the `Pager`
//! that produced them.  Because `alloc` and `free` both require `&mut Pager`,
//! the borrow checker statically prevents calling them while any page reference
//! is alive — making post-grow use-after-remap a **compile error**, not a
//! runtime hazard.
//!
//! ```compile_fail
//! # use mappedpages::{Pager, PageId};
//! let mut pager = Pager::<4096>::open("db.bin").unwrap();
//! let id = pager.alloc().unwrap();
//! let page = id.get(&pager).unwrap(); // borrows pager
//! pager.alloc().unwrap();             // ERROR: pager already borrowed
//! let _ = page;
//! ```

mod allocator;
mod concurrent;
mod error;
mod meta;
mod page;
mod pager;
mod protected;
mod sub_allocator;
#[cfg(test)]
mod tests;

pub use allocator::{BulkPageAllocator, PageAllocator, PageHandle};
pub use concurrent::{ConcurrentPager, ConcurrentPagerError, PagerReadGuard, PagerWriteGuard};
pub use error::MappedPageError;
pub use page::{MappedPage, PageId};
pub use pager::{AllocatedPageIter, AllocatedProtectedPageIter, Pager};
pub use protected::{ProtectedPageId, ProtectedPageWriter};
pub use sub_allocator::{SubPageAllocator, SubPageId};
