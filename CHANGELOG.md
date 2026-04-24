# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Concurrent access** — `ConcurrentPager<PAGE_SIZE>` wraps a `Pager` in an
  `Arc<RwLock<…>>` and is `Clone`, `Send`, and `Sync`.  `read()` acquires a
  shared `PagerReadGuard` (multiple threads may hold one simultaneously);
  `write()` acquires an exclusive `PagerWriteGuard` (no concurrent readers or
  writers).  Both guards dereference to the inner `Pager`, exposing the full
  existing API without adapter methods.  Non-blocking `try_read` / `try_write`
  variants and `into_inner` (recovers the `Pager` when the last clone is dropped)
  are also provided.  `ConcurrentPagerError` covers `Poisoned` (a thread panicked
  while holding the lock) and `WouldBlock` (non-blocking try variants).
  Backward-compatible: the existing `Pager` API is unchanged; `ConcurrentPager`
  is purely opt-in.

## [0.2.0] - 2026-04-24

### Added

- **`BulkPageAllocator` trait** — a new opt-in supertrait of `PageAllocator` that adds `alloc_bulk(count)` and `free_bulk(ids)` methods. Implemented by `Pager<N>` for both `PageId` and `ProtectedPageId` (delegating to the existing `alloc_bulk`/`free_bulk`/`alloc_protected_bulk`/`free_protected_bulk` methods), and by `SubPageAllocator<P, S>` for `SubPageId`. The trait is exported from the crate root. Generic code requiring bulk capability can use a `where A: BulkPageAllocator<H>` bound.
- **`SubPageAllocator` bulk operations** — `SubPageAllocator` now implements `BulkPageAllocator<SubPageId>`: `alloc_bulk` allocates `n` sub-pages and rolls back already-allocated entries on failure; `free_bulk` sorts and deduplicates the input, validates all ids atomically (bounds + in-use), then frees them in a single pass and returns any big pages whose all sub-slots are now free.
- `SubPageAllocator<PARENT_SIZE, SUB_SIZE>` and `SubPageId<PARENT_SIZE, SUB_SIZE>` — a convenience sub-page allocator that divides big pages from an inner `Pager<PARENT_SIZE>` into fixed-size sub-pages of `SUB_SIZE` bytes each. Both types are generic over the same const parameters and integrate with the existing `PageHandle` / `PageAllocator` traits. Up to 64 sub-pages per big page are supported (i.e. `PARENT_SIZE / SUB_SIZE ≤ 64`).
- Async I/O support — `Pager::alloc_async` and `Pager::free_async` methods for asynchronous page allocation and deallocation, available with the "async" feature flag. Enables integration with async runtimes like Tokio. Note: currently blocks the async runtime thread due to underlying memory map flush operations.
- **Bulk Operations API** — `Pager::alloc_bulk(count)` allocates multiple regular pages in a single crash-safe metadata commit, and `Pager::free_bulk(ids)` frees multiple regular pages in a single commit. Both validate atomically before modifying any state; on failure for `alloc_bulk` the in-memory bitmap is rolled back so the pager remains consistent. Async variants `alloc_bulk_async` and `free_bulk_async` are also available under the "async" feature flag.
- **Protected-page Bulk Operations** — `Pager::alloc_protected_bulk(count)` allocates multiple protected pages, rolling back on failure; `Pager::free_protected_bulk(ids)` validates all ids before freeing any. Async variants `alloc_protected_bulk_async` and `free_protected_bulk_async` are available under the "async" feature flag.
- **Page Iterators** — `Pager::iter_allocated_pages()` returns an `AllocatedPageIter` over regular data pages. Internal protected-page resources (directory block pages and backing pages for in-use protected entries) are excluded, so the iterator never exposes protected-page internals. `Pager::iter_allocated_protected_pages()` returns an `AllocatedProtectedPageIter` over in-use protected pages; regular data pages are never included. The two iterators are strictly disjoint. Both types are public and exported from the crate root. Both iterators hold an immutable borrow on the pager, so allocation and deallocation are statically prevented while either is alive.

### Changed

- `Pager`, `PageId`, `ProtectedPageId`, and `ProtectedPageWriter` are now generic over a const `PAGE_SIZE: usize`. A `PageId<1024>` cannot be passed to a `Pager<4096>` — the compiler rejects the mismatch. This is a source-level breaking change: existing call sites must be updated (see migration note below).
- `Pager::create` no longer takes a `page_size_log2: u32` argument. The page size is now the const generic: `Pager::<4096>::create("data.bin")`. The requirement that `PAGE_SIZE` be a power of two and at least 1024 is now enforced at compile time rather than returning a runtime `InvalidPageSize` error.
- `Pager::open` validates the on-disk page size against `PAGE_SIZE` and returns `InvalidPageSize` at runtime if they differ.

**Migration:** Replace `Pager::create(path, log2)` with `Pager::<{1 << log2}>::create(path)` and `Pager::open(path)` with `Pager::<PAGE_SIZE>::open(path)`. Type annotations on `PageId` and `ProtectedPageId` gain the same parameter (e.g. `PageId<4096>`), though in most cases the compiler infers it from context.

## [0.1.0] - 2026-04-18

### Added

- `Pager` — memory-mapped, file-backed fixed-size page manager with `create` and `open` constructors.
- `Pager::alloc` / `Pager::free` — allocate and free regular data pages; file grows automatically when space is exhausted.
- `PageId` and `MappedPage` — opaque page handle and unsized page-view type with borrow-checked lifetime safety.
- Crash-consistent double-buffered metadata commit: inactive metadata page is written and synced before the superblock pointer is flipped, so the active page is never overwritten in place.
- CRC32 checksums on every metadata page; on open, the library validates both copies and falls back to the alternate if one is corrupt.
- `ProtectedPageId`, `ProtectedPageWriter` — crash-consistent copy-on-write pages backed by two physical copies. Writes are staged via `ProtectedPageWriter::page_mut` and atomically promoted on `commit`.
- `Pager::alloc_protected` / `Pager::free_protected` — allocate and free protected pages; directory blocks tracking the A/B copy pairs are stored in page 0 with the same double-buffered commit guarantee.
- `PageHandle<A>` trait — common interface for borrowing page data from an allocator, implemented by both `PageId` and `ProtectedPageId`.
- `PageAllocator<H>` trait — generic allocator interface implemented by `Pager` for both `PageId` and `ProtectedPageId`.
- `MappedPageError` — structured error type covering I/O failures, corrupt superblock/metadata/directory, out-of-bounds access, double-free, invalid page size, and unavailable pager state.
- File layout: page 0 (superblock + directory index), pages 1–2 (metadata A/B), pages 3+ (data).
- Minimum page size of 1024 bytes (`page_size_log2 >= 10`).

[0.2.0]: https://github.com/williamwutq/mappedpages/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/williamwutq/mappedpages/releases/tag/v0.1.0
