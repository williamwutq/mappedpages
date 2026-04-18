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
//! let mut pager = Pager::open("db.bin").unwrap();
//! let id = pager.alloc().unwrap();
//! let page = id.get(&pager).unwrap(); // borrows pager
//! pager.alloc().unwrap();             // ERROR: pager already borrowed
//! let _ = page;
//! ```
//!
//! # Minimum page size
//!
//! Page size must be at least 1024 bytes (`page_size_log2 >= 10`).  This
//! ensures each metadata page has enough room for the header, a bitmap
//! large enough to be useful, and the trailing checksum.

mod error;
mod meta;
mod page;
mod pager;

pub use error::MappedPageError;
pub use page::{MappedPage, PageId};
pub use pager::Pager;
