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
- **Sub-page allocation** — `SubPageAllocator` divides big pages into smaller uniform sub-pages, so callers don't have to implement their own bitmap-based slab allocators on top of the raw pages.
- **Async I/O support** — async versions of allocation and deallocation methods are available with the "async" feature flag, enabling integration with async runtimes like Tokio.
- **Bulk operations** — `alloc_bulk(count)` and `free_bulk(ids)` allocate or free multiple pages in a single crash-safe metadata commit, reducing overhead for workloads that need many pages at once. `free_bulk` validates all ids atomically so no partial state change occurs on error.
- **Page iterator** — `iter_allocated_pages()` returns an `AllocatedPageIter` that traverses the allocation bitmap and yields every allocated data page, enabling efficient traversal for maintenance, backup, or analysis without loading any page data.

## File layout

```
Page 0  — Superblock   (magic, version, page size, active metadata selector + checksum)
Page 1  — Metadata A   (free bitmap, total pages, generation, checksum)
Page 2  — Metadata B   (same layout; alternate buffer for crash-safe commits)
Page 3+ — Data pages   (user-visible, returned by alloc)
```

The minimum page size is 1024 bytes; violating this or using a non-power-of-two `PAGE_SIZE` is a compile error.

**The page size is a permanent property of the file.** It is written to the superblock on `create` and validated against `PAGE_SIZE` on every subsequent `open`. All allocation — including sub-page allocation — happens within the data bytes of pages that already have this fixed size on disk.

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

### Sub-page allocation

The file format always uses a single fixed page size — the size is written to the superblock on `create` and checked on every `open`.  When your workload needs smaller granularity than the on-disk page size, `SubPageAllocator` handles the bookkeeping for you so you don't have to build your own bitmap-based slab allocator on top of raw pages.

`SubPageAllocator<PARENT_SIZE, SUB_SIZE>` wraps a `Pager<PARENT_SIZE>`, divides each big page it checks out into `PARENT_SIZE / SUB_SIZE` sub-slots, and exposes them as `SubPageId` handles that plug into the same `PageHandle` / `PageAllocator` traits.  Up to 64 sub-pages per big page are supported.

**Important:** sub-allocation state is in-memory only.  The on-disk file is unchanged — sub-slots live inside the data bytes of normal pages — but the free/used bitmasks are not written to disk.  On process restart, reconstruct your sub-allocation state from your own records before using sub-page handles again.

```rust
use mappedpages::{PageAllocator, PageHandle, Pager, SubPageAllocator};

// Wrap a pager; no big pages are checked out yet.
let pager = Pager::<4096>::create("data.bin")?;
let mut sub = SubPageAllocator::<4096, 512>::new(pager);

// Each sub-page is 512 bytes; 8 fit in one 4096-byte big page.
let id = sub.alloc()?;                                   // SubPageId<4096, 512>
sub.alloc_mut(id)?.as_bytes_mut().fill(0xAB);           // write 512 bytes
assert_eq!(id.get(&sub)?.len(), 512);

sub.free(id)?;   // when all 8 sub-slots in a big page are freed,
                 // the big page is returned to the inner pager
```

Recover the inner pager when you are done:

```rust
let pager: Pager<4096> = sub.into_pager();
```

### Async I/O Support

Async versions of allocation and deallocation methods are available when the "async" feature is enabled. These methods allow integration with async runtimes like Tokio.

Add to your `Cargo.toml`:

```toml
[dependencies]
mappedpages = { version = "0.1", features = ["async"] }
tokio = { version = "1", features = ["full"] }
```

Usage:

```rust
use mappedpages::Pager;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut pager = Pager::<4096>::create("data.bin")?;

    // Allocate asynchronously
    let id = pager.alloc_async().await?;

    // Use the page (synchronous access)
    {
        let page = id.get_mut(&mut pager)?;
        page.as_bytes_mut().fill(0xAB);
    }

    // Free asynchronously
    pager.free_async(id).await?;

    Ok(())
}
```

Note: Currently, the async methods block the async runtime thread due to underlying memory map flush operations. Future versions may provide truly non-blocking async I/O.

### Bulk operations

`alloc_bulk` and `free_bulk` perform multiple allocations or deallocations with only a single crash-safe metadata commit at the end, reducing overhead compared to individual `alloc`/`free` calls.

```rust
use mappedpages::Pager;

let mut pager = Pager::<4096>::create("data.bin")?;

// Allocate 10 pages in one commit.
let ids = pager.alloc_bulk(10)?;
assert_eq!(ids.len(), 10);

// Write to each page.
for id in &ids {
    id.get_mut(&mut pager)?.as_bytes_mut().fill(0xAB);
}

// Free all 10 pages in one commit.
// All ids are validated atomically before any pages are freed —
// a single invalid id returns an error and leaves all pages unchanged.
pager.free_bulk(ids)?;
```

Async variants are also available under the "async" feature flag:

```rust
let ids = pager.alloc_bulk_async(10).await?;
pager.free_bulk_async(ids).await?;
```

### Iterating allocated pages

`iter_allocated_pages` traverses the allocation bitmap and yields a `PageId` for each allocated data page. Reserved pages 0–2 are never included. The iterator holds an immutable borrow on the pager, so `alloc` and `free` cannot be called while it is alive.

```rust
use mappedpages::Pager;

let mut pager = Pager::<4096>::create("data.bin")?;
pager.alloc_bulk(3)?;

// Collect all allocated page ids.
let allocated: Vec<_> = pager.iter_allocated_pages().collect();
assert_eq!(allocated.len(), 3);

// Iterate and read each page.
for id in pager.iter_allocated_pages() {
    let page = id.get(&pager)?;
    println!("page {}: first byte = {}", id.0, page.as_bytes()[0]);
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
| `alloc_bulk` | `(&mut self, usize) -> Result<Vec<PageId<PAGE_SIZE>>>` | Allocate `n` pages in one commit; grows as needed |
| `free_bulk` | `(&mut self, Vec<PageId<PAGE_SIZE>>) -> Result<()>` | Free multiple pages in one commit; validates all ids atomically |
| `iter_allocated_pages` | `(&self) -> AllocatedPageIter<'_, PAGE_SIZE>` | Iterator over all currently allocated data pages |
| `alloc_async` | `(&mut self) -> Result<PageId<PAGE_SIZE>>` | Async version of `alloc` (requires "async" feature) |
| `free_async` | `(&mut self, PageId<PAGE_SIZE>) -> Result<()>` | Async version of `free` (requires "async" feature) |
| `alloc_bulk_async` | `(&mut self, usize) -> Result<Vec<PageId<PAGE_SIZE>>>` | Async version of `alloc_bulk` (requires "async" feature) |
| `free_bulk_async` | `(&mut self, Vec<PageId<PAGE_SIZE>>) -> Result<()>` | Async version of `free_bulk` (requires "async" feature) |
| `alloc_protected` | `(&mut self) -> Result<ProtectedPageId<PAGE_SIZE>>` | Allocate a crash-consistent copy-on-write page |
| `free_protected` | `(&mut self, ProtectedPageId<PAGE_SIZE>) -> Result<()>` | Free a protected page and both its backing copies |
| `alloc_protected_async` | `(&mut self) -> Result<ProtectedPageId<PAGE_SIZE>>` | Async version of `alloc_protected` (requires "async" feature) |
| `free_protected_async` | `(&mut self, ProtectedPageId<PAGE_SIZE>) -> Result<()>` | Async version of `free_protected` (requires "async" feature) |
| `page_size` | `(&self) -> usize` | Page size in bytes (always equal to `PAGE_SIZE`) |
| `page_count` | `(&self) -> u64` | Total pages in the file, including reserved pages 0–2 |
| `free_page_count` | `(&self) -> u64` | Pages currently available for allocation |

### `AllocatedPageIter<'_, PAGE_SIZE>`

An iterator over allocated data pages, yielded by `Pager::iter_allocated_pages`.  Implements `Iterator<Item = PageId<PAGE_SIZE>>`.  Reserved pages 0–2 are never yielded.  Holds an immutable borrow on the `Pager` for its lifetime.

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

### `SubPageAllocator<PARENT_SIZE, SUB_SIZE>`

Divides big pages from a `Pager<PARENT_SIZE>` into sub-pages of `SUB_SIZE` bytes.  Owns the inner pager.  Sub-allocation state is **in-memory only** — not persisted and not crash-consistent.

Compile-time constraints: `SUB_SIZE` must be a power of two, `PARENT_SIZE` must be divisible by `SUB_SIZE`, `SUB_SIZE < PARENT_SIZE`, and `PARENT_SIZE / SUB_SIZE ≤ 64`.

| Method | Signature | Description |
|---|---|---|
| `new` | `(Pager<PARENT_SIZE>) -> Self` | Wrap a pager; no big pages are checked out until the first `alloc` |
| `alloc` | `(&mut self) -> Result<SubPageId<PARENT_SIZE, SUB_SIZE>>` | Allocate one sub-page; checks out a new big page from the inner pager when needed |
| `free` | `(&mut self, SubPageId<...>) -> Result<()>` | Free a sub-page; returns the big page to the inner pager when all its sub-slots are free |
| `pager` | `(&self) -> &Pager<PARENT_SIZE>` | Borrow the inner pager (e.g. to query `page_count`) |
| `into_pager` | `(self) -> Pager<PARENT_SIZE>` | Consume this allocator and recover the inner pager |

### `SubPageId<PARENT_SIZE, SUB_SIZE>`

Opaque handle to one allocated sub-page.  Cheap to copy.  Can only be used with a `SubPageAllocator` of matching `PARENT_SIZE` and `SUB_SIZE`.

| Method    | Signature                                                              | Description |
|-----------|------------------------------------------------------------------------|-------------|
| `get`     | `(&self, &'a SubPageAllocator<...>) -> Result<&'a MappedPage>`         | Immutably borrow the sub-page (`len()` == `SUB_SIZE`) |
| `get_mut` | `(&self, &'a mut SubPageAllocator<...>) -> Result<&'a mut MappedPage>` | Mutably borrow the sub-page |

### `PageAllocator` / `PageHandle` traits

`PageHandle<A>` is implemented by `PageId<N>`, `ProtectedPageId<N>`, and `SubPageId<P, S>` for their respective allocator types.  `PageAllocator<H>` is implemented by `Pager<N>` for the first two and by `SubPageAllocator<P, S>` for the third, enabling generic allocator code across all three page types.

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
