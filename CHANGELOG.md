# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/williamwutq/mappedpages/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/williamwutq/mappedpages/releases/tag/v0.1.0
