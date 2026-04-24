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
//! [`Pager::alloc_bulk`] and [`Pager::free_bulk`] allocate or free multiple pages
//! in a single crash-safe metadata commit, reducing overhead for workloads that
//! need many pages at once.  [`free_bulk`](Pager::free_bulk) validates all ids
//! atomically before touching the bitmap — a single invalid id causes the whole
//! call to fail without modifying any state.
//!
//! # Page iteration
//!
//! [`Pager::iter_allocated_pages`] returns an [`AllocatedPageIter`] that traverses
//! the allocation bitmap and yields a [`PageId`] for each allocated data page.
//! Reserved pages 0–2 are never included.  The iterator holds an immutable borrow
//! on the pager, so allocation and deallocation are statically prevented while it
//! is alive.
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
//! [`ProtectedPageId`].
//!
//! **Sub-allocation state is in-memory only.**  The on-disk file is unchanged —
//! sub-slots live inside the data bytes of ordinary pages, and the free/used
//! bitmasks are never written to disk.  On process restart, callers must
//! reconstruct which sub-slots are in use from their own records before
//! issuing sub-page handles again.
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
mod error;
mod meta;
mod page;
mod pager;
mod protected;
mod sub_allocator;
#[cfg(test)]
mod tests;

pub use allocator::{PageAllocator, PageHandle};
pub use error::MappedPageError;
pub use page::{MappedPage, PageId};
pub use pager::{AllocatedPageIter, Pager};
pub use protected::{ProtectedPageId, ProtectedPageWriter};
pub use sub_allocator::{SubPageAllocator, SubPageId};
