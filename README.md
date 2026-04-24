# mappedpages

A crash-consistent, memory-mapped, file-backed fixed-size page provider for Rust.

`mappedpages` manages a binary file divided into fixed-size pages, addressable by `PageId`.  It is intended as a low-level building block for higher-level allocators and storage systems.

[![Crates.io](https://img.shields.io/crates/v/mappedpages)](https://crates.io/crates/mappedpages)
[![Docs.rs](https://img.shields.io/docsrs/mappedpages)](https://docs.rs/mappedpages)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

## Features

- **Compile-time page-size safety** — `Pager`, `PageId`, and `ProtectedPageId` are all generic over a const `PAGE_SIZE: usize`. A `PageId<1024>` cannot be passed to a `Pager<4096>` — the compiler rejects the mismatch.
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

The minimum page size is 1024 bytes; violating this or using a non-power-of-two `PAGE_SIZE` is a compile error.

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
mappedpages = "0.1"
```

## Usage

### Creating and opening a pager

The page size is specified as a const generic on `Pager`.  It must be a power of two and at least 1024 — both constraints are enforced at compile time.

```rust
use mappedpages::Pager;

// Create a new file with 4096-byte pages.
let mut pager = Pager::<4096>::create("data.bin")?;

// Open an existing file — returns InvalidPageSize if the on-disk size differs.
let mut pager = Pager::<4096>::open("data.bin")?;
```

### Allocating and accessing pages

`PageId<PAGE_SIZE>` is an opaque handle tied to the same page size as the `Pager` that produced it.  Using a handle with the wrong pager is a compile error.

```rust
use mappedpages::{Pager, PageId};

let mut pager = Pager::<4096>::create("data.bin")?;

// Allocate a new page.
let id: PageId<4096> = pager.alloc()?;

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

let mut pager = Pager::<4096>::create("data.bin")?;

// Allocate a protected page.
let id: ProtectedPageId<4096> = pager.alloc_protected()?;

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

Both `PageId` and `ProtectedPageId` implement the `PageHandle<Pager<N>>` trait, and `Pager<N>` implements `PageAllocator` for both.  This allows generic code to work with either page type:

```rust
use mappedpages::{PageAllocator, PageHandle, Pager, PageId};

fn fill_pages<H, const N: usize>(pager: &mut Pager<N>, n: usize) -> Vec<H>
where
    H: PageHandle<Pager<N>>,
    Pager<N>: PageAllocator<H>,
{
    (0..n).map(|_| pager.alloc().unwrap()).collect()
}
```

## API

### `Pager<PAGE_SIZE>`

The central type.  All page handles hold a borrow on the `Pager` that produced them.

| Method | Signature | Description |
|---|---|---|
| `create` | `(path) -> Result<Self>` | Create a new file; fails if it already exists |
| `open` | `(path) -> Result<Self>` | Open and validate an existing file; fails if the on-disk page size ≠ `PAGE_SIZE` |
| `alloc` | `(&mut self) -> Result<PageId<PAGE_SIZE>>` | Allocate a regular page; grows the file if needed |
| `free` | `(&mut self, PageId<PAGE_SIZE>) -> Result<()>` | Free a regular page |
| `alloc_protected` | `(&mut self) -> Result<ProtectedPageId<PAGE_SIZE>>` | Allocate a crash-consistent copy-on-write page |
| `free_protected` | `(&mut self, ProtectedPageId<PAGE_SIZE>) -> Result<()>` | Free a protected page and both its backing copies |
| `page_size` | `(&self) -> usize` | Page size in bytes (always equal to `PAGE_SIZE`) |
| `page_count` | `(&self) -> u64` | Total pages in the file, including reserved pages 0–2 |
| `free_page_count` | `(&self) -> u64` | Pages currently available for allocation |

### `PageId<PAGE_SIZE>`

Opaque handle to a regular data page.  Cheap to copy.  Can only be used with a `Pager<PAGE_SIZE>` of the same size.

| Method | Signature | Description |
|---|---|---|
| `get` | `(&self, &'a Pager<PAGE_SIZE>) -> Result<&'a MappedPage>` | Immutably borrow the page |
| `get_mut` | `(&self, &'a mut Pager<PAGE_SIZE>) -> Result<&'a mut MappedPage>` | Mutably borrow the page |

### `MappedPage`

Unsized view into one page of the memory map (analogous to `str` or `Path`).  Always held behind a reference.

| Method | Signature | Description |
|---|---|---|
| `as_bytes` | `(&self) -> &[u8]` | Raw byte slice of the page |
| `as_bytes_mut` | `(&mut self) -> &mut [u8]` | Mutable raw byte slice |
| `len` | `(&self) -> usize` | Page size in bytes |
| `is_empty` | `(&self) -> bool` | Always `false` for valid pages |

### `ProtectedPageId<PAGE_SIZE>`

Opaque handle to a crash-consistent copy-on-write page.  Cheap to copy.  Can only be used with a `Pager<PAGE_SIZE>` of the same size.

| Method | Signature | Description |
|---|---|---|
| `get` | `(&self, &'a Pager<PAGE_SIZE>) -> Result<&'a MappedPage>` | Read the active copy |
| `get_mut` | `(&self, &'a mut Pager<PAGE_SIZE>) -> Result<ProtectedPageWriter<'a, PAGE_SIZE>>` | Begin a staged write |

### `ProtectedPageWriter<'_, PAGE_SIZE>`

In-progress write to a protected page.  Dropping without `commit` leaves the active copy unchanged.

| Method | Signature | Description |
|---|---|---|
| `page_mut` | `(&mut self) -> &mut MappedPage` | Mutable view of the page being written |
| `commit` | `(self) -> Result<()>` | Flush and atomically promote the write to active |

### `PageAllocator` / `PageHandle` traits

`PageHandle<A>` is implemented by `PageId<N>` and `ProtectedPageId<N>` for `A = Pager<N>`.  `PageAllocator<H>` is implemented by `Pager<N>` for both handle types, enabling generic allocator code.

## Error handling

All fallible operations return `Result<_, MappedPageError>`.  Notable variants:

| Variant               | Meaning                                                              |
|-----------------------|----------------------------------------------------------------------|
| `InvalidPageSize`     | On-disk page size does not match `PAGE_SIZE` (open); non-power-of-two or < 1024 is a compile error (create) |
| `CorruptSuperblock`   | Unrecognised magic or file too small                                 |
| `CorruptMetadata`     | Both metadata copies failed checksum                                 |
| `OutOfBounds`         | `PageId` refers to a non-existent page                               |
| `DoubleFree`          | Freeing an already-free page                                         |
| `Unavailable`         | Pager is unusable after a failed remap; reopen the file              |
