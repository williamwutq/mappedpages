# mappedpages

A crash-consistent, memory-mapped, file-backed fixed-size page provider for Rust.

`mappedpages` manages a binary file divided into fixed-size pages, addressable by `PageId`.  It is intended as a low-level building block for higher-level allocators and storage systems.

[![Crates.io](https://img.shields.io/crates/v/mappedpages)](https://crates.io/crates/mappedpages)
[![Docs.rs](https://img.shields.io/docsrs/mappedpages)](https://docs.rs/mappedpages)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

## Features

- **Crash consistency** — every allocation state change is committed via a double-buffered write: the inactive metadata page is written and synced first, then the superblock pointer is flipped and synced. The active metadata page is never overwritten in place.
- **Protected pages** — copy-on-write pages backed by two physical pages. Writes are staged in the inactive copy and atomically promoted on `commit`, surviving any crash between the two.
- **Borrow-checked safety** — `&MappedPage` and `&mut MappedPage` hold a borrow on the `Pager` that produced them. `alloc` and `free` both require `&mut Pager`, so the borrow checker statically prevents accessing a page reference after a remap — a compile error, not a runtime hazard.
- **Dynamic growth** — the file grows automatically when space is exhausted, with safe recovery if a remap fails mid-grow.
- **CRC32 checksums** — every metadata page and directory block is protected by a CRC32 checksum.  On open, the library validates both copies and falls back to the alternate if one is corrupt.

## File layout

```
Page 0  — Superblock   (magic, version, page size, active metadata selector + checksum)
Page 1  — Metadata A   (free bitmap, total pages, generation, checksum)
Page 2  — Metadata B   (same layout; alternate buffer for crash-safe commits)
Page 3+ — Data pages   (user-visible, returned by alloc)
```

The minimum page size is 1024 bytes (`page_size_log2 >= 10`).

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
mappedpages = "0.1"
```

## Usage

### Creating and opening a pager

```rust
use mappedpages::Pager;

// Create a new file with 4096-byte pages (2^12).
let mut pager = Pager::create("data.bin", 12)?;

// Open an existing file.
let mut pager = Pager::open("data.bin")?;
```

### Allocating and accessing pages

```rust
use mappedpages::{Pager, PageId};

let mut pager = Pager::create("data.bin", 12)?;

// Allocate a new page.
let id: PageId = pager.alloc()?;

// Write to the page — requires &mut Pager.
{
    let page = id.get_mut(&mut pager)?;
    page.as_bytes_mut().fill(0xAB);
} // mutable borrow released here

// Read from the page — requires &Pager.
{
    let page = id.get(&pager)?;
    println!("{:?}", &page.as_bytes()[..4]);
}

// Free the page when done.
pager.free(id)?;
```

### Protected (crash-consistent) pages

Protected pages use copy-on-write: a write is staged in the inactive physical copy and only becomes visible after an explicit `commit`.  Dropping a `ProtectedPageWriter` without committing discards the write.

```rust
use mappedpages::{Pager, ProtectedPageId};

let mut pager = Pager::create("data.bin", 12)?;

// Allocate a protected page.
let id: ProtectedPageId = pager.alloc_protected()?;

// Stage a write.
{
    let mut writer = id.get_mut(&mut pager)?;
    writer.page_mut().as_bytes_mut().fill(0xFF);
    writer.commit()?; // atomically makes the write durable
}

// Read the active copy.
{
    let page = id.get(&pager)?;
    assert_eq!(page.as_bytes()[0], 0xFF);
}
```

### `PageAllocator` trait

Both `PageId` and `ProtectedPageId` implement the `PageHandle<Pager>` trait, and `Pager` implements `PageAllocator` for both.  This allows generic code to work with either page type:

```rust
use mappedpages::{PageAllocator, PageHandle, Pager, PageId};

fn fill_pages<H>(pager: &mut Pager, n: usize) -> Vec<H>
where
    H: PageHandle<Pager>,
    Pager: PageAllocator<H>,
{
    (0..n).map(|_| pager.alloc().unwrap()).collect()
}
```

## API

### `Pager`

The central type.  All page handles hold a borrow on the `Pager` that produced them.

| Method | Signature | Description |
|---|---|---|
| `create` | `(path, page_size_log2: u32) -> Result<Self>` | Create a new file; fails if it already exists |
| `open` | `(path) -> Result<Self>` | Open and validate an existing file |
| `alloc` | `(&mut self) -> Result<PageId>` | Allocate a regular page; grows the file if needed |
| `free` | `(&mut self, PageId) -> Result<()>` | Free a regular page |
| `alloc_protected` | `(&mut self) -> Result<ProtectedPageId>` | Allocate a crash-consistent copy-on-write page |
| `free_protected` | `(&mut self, ProtectedPageId) -> Result<()>` | Free a protected page and both its backing copies |
| `page_size` | `(&self) -> usize` | Page size in bytes |
| `page_count` | `(&self) -> u64` | Total pages in the file, including reserved pages 0–2 |
| `free_page_count` | `(&self) -> u64` | Pages currently available for allocation |

### `PageId`

Opaque handle to a regular data page.  Cheap to copy.

| Method | Signature | Description |
|---|---|---|
| `get` | `(&self, &'a Pager) -> Result<&'a MappedPage>` | Immutably borrow the page |
| `get_mut` | `(&self, &'a mut Pager) -> Result<&'a mut MappedPage>` | Mutably borrow the page |

### `MappedPage`

Unsized view into one page of the memory map (analogous to `str` or `Path`).  Always held behind a reference.

| Method | Signature | Description |
|---|---|---|
| `as_bytes` | `(&self) -> &[u8]` | Raw byte slice of the page |
| `as_bytes_mut` | `(&mut self) -> &mut [u8]` | Mutable raw byte slice |
| `len` | `(&self) -> usize` | Page size in bytes |
| `is_empty` | `(&self) -> bool` | Always `false` for valid pages |

### `ProtectedPageId`

Opaque handle to a crash-consistent copy-on-write page.  Cheap to copy.

| Method | Signature | Description |
|---|---|---|
| `get` | `(&self, &'a Pager) -> Result<&'a MappedPage>` | Read the active copy |
| `get_mut` | `(&self, &'a mut Pager) -> Result<ProtectedPageWriter<'a>>` | Begin a staged write |

### `ProtectedPageWriter<'_>`

In-progress write to a protected page.  Dropping without `commit` leaves the active copy unchanged.

| Method | Signature | Description |
|---|---|---|
| `page_mut` | `(&mut self) -> &mut MappedPage` | Mutable view of the page being written |
| `commit` | `(self) -> Result<()>` | Flush and atomically promote the write to active |

### `PageAllocator` / `PageHandle` traits

`PageHandle<A>` is implemented by `PageId` and `ProtectedPageId`.  `PageAllocator<H>` is implemented by `Pager` for both handle types, enabling generic allocator code.

## Error handling

All fallible operations return `Result<_, MappedPageError>`.  Notable variants:

| Variant               | Meaning                                                   |
|-----------------------|-----------------------------------------------------------|
| `InvalidPageSize`     | `page_size_log2 < 10`                                     |
| `CorruptSuperblock`   | Unrecognised magic or file too small                      |
| `CorruptMetadata`     | Both metadata copies failed checksum                      |
| `OutOfBounds`         | `PageId` refers to a non-existent page                    |
| `DoubleFree`          | Freeing an already-free page                              |
| `Unavailable`         | Pager is unusable after a failed remap; reopen the file   |

