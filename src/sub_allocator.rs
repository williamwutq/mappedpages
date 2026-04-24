//! Sub-page allocator: divides big pages from a `Pager<PARENT_SIZE>` into
//! fixed-size sub-pages of `SUB_SIZE` bytes each.
//!
//! Each big page is divided into `N = PARENT_SIZE / SUB_SIZE` sub-slots, tracked
//! with a `u64` bitmask (one bit per slot, so N ≤ 64 is required).  When all
//! sub-slots of a big page are freed the big page is returned to the inner pager.
//!
//! Sub-allocation state is **in-memory only**.  It is not persisted to disk and
//! is not reconstructed when a pager file is reopened.  Callers are responsible
//! for rebuilding their sub-allocation state after a reopen.

use crate::allocator::{BulkPageAllocator, PageAllocator, PageHandle};
use crate::error::MappedPageError;
use crate::page::{MappedPage, PageId};
use crate::pager::Pager;

// ── Private helpers ───────────────────────────────────────────────────────────

/// One big page checked out from the inner pager, together with its sub-slot
/// bitmask.  Bit `i` = 1 means sub-slot `i` is currently allocated.
struct BigPageSlot<const PARENT_SIZE: usize> {
    id: PageId<PARENT_SIZE>,
    used: u64,
}

// ── SubPageId ─────────────────────────────────────────────────────────────────

/// An opaque handle to one allocated sub-page inside a `SubPageAllocator`.
///
/// The const generics `PARENT_SIZE` and `SUB_SIZE` ensure that a
/// `SubPageId<4096, 512>` cannot be passed to a `SubPageAllocator<4096, 256>` —
/// the compiler rejects the mismatch.
///
/// Fields are private because `slot_index` encodes an implementation-internal
/// position in the allocator's slot vec, not a stable on-disk address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SubPageId<const PARENT_SIZE: usize, const SUB_SIZE: usize> {
    /// Index into `SubPageAllocator::slots`.
    pub(crate) slot_index: u32,
    /// Which sub-slot within that big page (0 .. PARENT_SIZE / SUB_SIZE).
    pub(crate) sub_index: u32,
}

#[cfg(test)]
impl<const PARENT_SIZE: usize, const SUB_SIZE: usize> SubPageId<PARENT_SIZE, SUB_SIZE> {
    /// Construct an arbitrary `SubPageId` for testing error paths.
    pub(crate) fn new_raw(slot_index: u32, sub_index: u32) -> Self {
        Self {
            slot_index,
            sub_index,
        }
    }
}

// ── SubPageAllocator ──────────────────────────────────────────────────────────

/// A sub-page allocator that divides big pages from an inner `Pager<PARENT_SIZE>`
/// into sub-pages of `SUB_SIZE` bytes each.
///
/// # Compile-time constraints
///
/// - `SUB_SIZE` must be a power of two.
/// - `PARENT_SIZE` must be divisible by `SUB_SIZE`.
/// - `SUB_SIZE` must be strictly less than `PARENT_SIZE`.
/// - `PARENT_SIZE / SUB_SIZE` must be at most 64 (fits in a `u64` bitmask).
///
/// Violating any of these is a **compile error** via `const { assert!(...) }`.
///
/// # Persistence
///
/// Sub-allocation state is in-memory only.  The inner `Pager`'s file is updated
/// when big pages are allocated or freed, but the sub-slot bitmasks are not
/// written to disk.  Reconstruct sub-allocation state after reopening the file.
pub struct SubPageAllocator<const PARENT_SIZE: usize, const SUB_SIZE: usize> {
    pager: Pager<PARENT_SIZE>,
    /// One entry per big page currently (or previously) checked out.
    /// `None` is a tombstone: the big page was fully freed and returned to the
    /// inner pager; its slot index may be reused by a future `alloc`.
    slots: Vec<Option<BigPageSlot<PARENT_SIZE>>>,
}

impl<const PARENT_SIZE: usize, const SUB_SIZE: usize> SubPageAllocator<PARENT_SIZE, SUB_SIZE> {
    /// Bitmask value when every sub-slot in a big page is in use.
    ///
    /// Handles the N = 64 edge case where `1u64 << 64` would be undefined
    /// behaviour — the branch is constant-folded away by the compiler.
    const fn full_mask() -> u64 {
        if PARENT_SIZE / SUB_SIZE == 64 {
            u64::MAX
        } else {
            (1u64 << (PARENT_SIZE / SUB_SIZE)) - 1
        }
    }

    /// Create a new `SubPageAllocator` wrapping `pager`.
    ///
    /// No pages are allocated from `pager` until the first call to [`alloc`](Self::alloc).
    pub fn new(pager: Pager<PARENT_SIZE>) -> Self {
        const {
            assert!(
                SUB_SIZE.is_power_of_two(),
                "SUB_SIZE must be a power of two"
            )
        };
        const {
            assert!(
                PARENT_SIZE.is_multiple_of(SUB_SIZE),
                "PARENT_SIZE must be divisible by SUB_SIZE"
            )
        };
        const {
            assert!(
                SUB_SIZE < PARENT_SIZE,
                "SUB_SIZE must be less than PARENT_SIZE"
            )
        };
        const {
            assert!(
                PARENT_SIZE / SUB_SIZE <= 64,
                "PARENT_SIZE / SUB_SIZE must be at most 64"
            )
        };
        Self {
            pager,
            slots: Vec::new(),
        }
    }

    /// Borrow the inner pager (e.g. to query `page_count` or `free_page_count`).
    pub fn pager(&self) -> &Pager<PARENT_SIZE> {
        &self.pager
    }

    /// Consume this allocator and return the inner pager.
    ///
    /// Any big pages that still have live sub-slots allocated remain checked out
    /// in the pager.  Callers are responsible for freeing them if needed.
    pub fn into_pager(self) -> Pager<PARENT_SIZE> {
        self.pager
    }
}

// ── PageAllocator impl ────────────────────────────────────────────────────────

impl<const PARENT_SIZE: usize, const SUB_SIZE: usize>
    PageAllocator<SubPageId<PARENT_SIZE, SUB_SIZE>> for SubPageAllocator<PARENT_SIZE, SUB_SIZE>
{
    type Error = MappedPageError;

    fn alloc(&mut self) -> Result<SubPageId<PARENT_SIZE, SUB_SIZE>, MappedPageError> {
        const {
            assert!(
                SUB_SIZE.is_power_of_two(),
                "SUB_SIZE must be a power of two"
            )
        };
        const {
            assert!(
                PARENT_SIZE.is_multiple_of(SUB_SIZE),
                "PARENT_SIZE must be divisible by SUB_SIZE"
            )
        };
        const {
            assert!(
                SUB_SIZE < PARENT_SIZE,
                "SUB_SIZE must be less than PARENT_SIZE"
            )
        };
        const {
            assert!(
                PARENT_SIZE / SUB_SIZE <= 64,
                "PARENT_SIZE / SUB_SIZE must be at most 64"
            )
        };

        let full = Self::full_mask();

        // Pass 1: find a partially-used big page with a free sub-slot.
        for (i, slot_opt) in self.slots.iter_mut().enumerate() {
            if let Some(slot) = slot_opt
                && slot.used != full
            {
                let bit = slot.used.trailing_ones();
                slot.used |= 1u64 << bit;
                return Ok(SubPageId {
                    slot_index: i as u32,
                    sub_index: bit,
                });
            }
        }

        // Pass 2: need a fresh big page; allocate it from the inner pager.
        let big_id = self.pager.alloc()?;
        let new_slot = BigPageSlot {
            id: big_id,
            used: 1u64,
        };

        // Reuse a tombstone slot if one exists.
        for (i, slot_opt) in self.slots.iter_mut().enumerate() {
            if slot_opt.is_none() {
                *slot_opt = Some(new_slot);
                return Ok(SubPageId {
                    slot_index: i as u32,
                    sub_index: 0,
                });
            }
        }

        // No tombstone; grow the vec.
        let idx = self.slots.len();
        self.slots.push(Some(new_slot));
        Ok(SubPageId {
            slot_index: idx as u32,
            sub_index: 0,
        })
    }

    fn free(&mut self, id: SubPageId<PARENT_SIZE, SUB_SIZE>) -> Result<(), MappedPageError> {
        const {
            assert!(
                SUB_SIZE.is_power_of_two(),
                "SUB_SIZE must be a power of two"
            )
        };
        const {
            assert!(
                PARENT_SIZE.is_multiple_of(SUB_SIZE),
                "PARENT_SIZE must be divisible by SUB_SIZE"
            )
        };
        const {
            assert!(
                SUB_SIZE < PARENT_SIZE,
                "SUB_SIZE must be less than PARENT_SIZE"
            )
        };
        const {
            assert!(
                PARENT_SIZE / SUB_SIZE <= 64,
                "PARENT_SIZE / SUB_SIZE must be at most 64"
            )
        };

        if id.sub_index as usize >= PARENT_SIZE / SUB_SIZE {
            return Err(MappedPageError::OutOfBounds);
        }

        let slot_opt = self
            .slots
            .get_mut(id.slot_index as usize)
            .ok_or(MappedPageError::OutOfBounds)?;

        let slot = slot_opt.as_mut().ok_or(MappedPageError::DoubleFree)?;

        let mask = 1u64 << id.sub_index;
        if slot.used & mask == 0 {
            return Err(MappedPageError::DoubleFree);
        }
        slot.used &= !mask;

        if slot.used == 0 {
            // All sub-slots free: set tombstone first, then release the big page.
            let big_id = slot.id;
            *slot_opt = None;
            self.pager.free(big_id)?;
        }
        Ok(())
    }
}

// ── BulkPageAllocator impl ──────────────────────────────────────────────────────

impl<const PARENT_SIZE: usize, const SUB_SIZE: usize>
    BulkPageAllocator<SubPageId<PARENT_SIZE, SUB_SIZE>>
    for SubPageAllocator<PARENT_SIZE, SUB_SIZE>
{
    fn alloc_bulk(
        &mut self,
        count: usize,
    ) -> Result<Vec<SubPageId<PARENT_SIZE, SUB_SIZE>>, MappedPageError> {
        if count == 0 {
            return Ok(Vec::new());
        }
        let mut ids = Vec::with_capacity(count);
        for _ in 0..count {
            match <Self as PageAllocator<SubPageId<PARENT_SIZE, SUB_SIZE>>>::alloc(self) {
                Ok(id) => ids.push(id),
                Err(e) => {
                    for id in ids.drain(..) {
                        let _ = <Self as PageAllocator<SubPageId<PARENT_SIZE, SUB_SIZE>>>::free(
                            self, id,
                        );
                    }
                    return Err(e);
                }
            }
        }
        Ok(ids)
    }

    fn free_bulk(
        &mut self,
        mut ids: Vec<SubPageId<PARENT_SIZE, SUB_SIZE>>,
    ) -> Result<(), MappedPageError> {
        if ids.is_empty() {
            return Ok(());
        }

        // Sort and detect duplicates before touching any state.
        ids.sort_unstable();
        for w in ids.windows(2) {
            if w[0] == w[1] {
                return Err(MappedPageError::DoubleFree);
            }
        }

        // Validate every id atomically: bounds check and in-use check.
        for id in &ids {
            if id.sub_index as usize >= PARENT_SIZE / SUB_SIZE {
                return Err(MappedPageError::OutOfBounds);
            }
            let slot = self
                .slots
                .get(id.slot_index as usize)
                .ok_or(MappedPageError::OutOfBounds)?;
            let slot = slot.as_ref().ok_or(MappedPageError::DoubleFree)?;
            if slot.used & (1u64 << id.sub_index) == 0 {
                return Err(MappedPageError::DoubleFree);
            }
        }

        // All ids are valid: clear their bits and collect big pages that
        // become fully free so they can be returned to the inner pager.
        let mut to_release: Vec<PageId<PARENT_SIZE>> = Vec::new();
        for id in &ids {
            let slot_opt = &mut self.slots[id.slot_index as usize];
            let slot = slot_opt.as_mut().unwrap(); // safe: validated above
            slot.used &= !(1u64 << id.sub_index);
            if slot.used == 0 {
                let big_id = slot.id;
                *slot_opt = None;
                to_release.push(big_id);
            }
        }
        for big_id in to_release {
            self.pager.free(big_id)?;
        }
        Ok(())
    }
}

// ── PageHandle impl ───────────────────────────────────────────────────────────

impl<const PARENT_SIZE: usize, const SUB_SIZE: usize>
    PageHandle<SubPageAllocator<PARENT_SIZE, SUB_SIZE>> for SubPageId<PARENT_SIZE, SUB_SIZE>
{
    type Error = MappedPageError;
    type Mut<'a>
        = &'a mut MappedPage
    where
        SubPageAllocator<PARENT_SIZE, SUB_SIZE>: 'a,
        Self: 'a;

    fn get<'a>(
        &self,
        allocator: &'a SubPageAllocator<PARENT_SIZE, SUB_SIZE>,
    ) -> Result<&'a MappedPage, MappedPageError> {
        const {
            assert!(
                SUB_SIZE.is_power_of_two(),
                "SUB_SIZE must be a power of two"
            )
        };
        const {
            assert!(
                PARENT_SIZE.is_multiple_of(SUB_SIZE),
                "PARENT_SIZE must be divisible by SUB_SIZE"
            )
        };
        const {
            assert!(
                SUB_SIZE < PARENT_SIZE,
                "SUB_SIZE must be less than PARENT_SIZE"
            )
        };
        const {
            assert!(
                PARENT_SIZE / SUB_SIZE <= 64,
                "PARENT_SIZE / SUB_SIZE must be at most 64"
            )
        };

        if self.sub_index as usize >= PARENT_SIZE / SUB_SIZE {
            return Err(MappedPageError::OutOfBounds);
        }

        let slot = allocator
            .slots
            .get(self.slot_index as usize)
            .and_then(Option::as_ref)
            .ok_or(MappedPageError::OutOfBounds)?;

        let big_page = slot.id.get(&allocator.pager)?;
        let start = self.sub_index as usize * SUB_SIZE;
        Ok(unsafe { MappedPage::from_slice(&big_page.as_bytes()[start..start + SUB_SIZE]) })
    }

    fn get_mut<'a>(
        &self,
        allocator: &'a mut SubPageAllocator<PARENT_SIZE, SUB_SIZE>,
    ) -> Result<&'a mut MappedPage, MappedPageError> {
        const {
            assert!(
                SUB_SIZE.is_power_of_two(),
                "SUB_SIZE must be a power of two"
            )
        };
        const {
            assert!(
                PARENT_SIZE.is_multiple_of(SUB_SIZE),
                "PARENT_SIZE must be divisible by SUB_SIZE"
            )
        };
        const {
            assert!(
                SUB_SIZE < PARENT_SIZE,
                "SUB_SIZE must be less than PARENT_SIZE"
            )
        };
        const {
            assert!(
                PARENT_SIZE / SUB_SIZE <= 64,
                "PARENT_SIZE / SUB_SIZE must be at most 64"
            )
        };

        if self.sub_index as usize >= PARENT_SIZE / SUB_SIZE {
            return Err(MappedPageError::OutOfBounds);
        }

        // Extract big_id (Copy) while slots is borrowed, then drop that borrow
        // before mutably borrowing pager — required to avoid overlapping field
        // borrows on the same SubPageAllocator.
        let big_id = allocator
            .slots
            .get(self.slot_index as usize)
            .and_then(Option::as_ref)
            .ok_or(MappedPageError::OutOfBounds)?
            .id;

        let big_page = big_id.get_mut(&mut allocator.pager)?;
        let start = self.sub_index as usize * SUB_SIZE;
        Ok(unsafe {
            MappedPage::from_slice_mut(&mut big_page.as_bytes_mut()[start..start + SUB_SIZE])
        })
    }
}
