use crate::{MappedPageError, Pager};

/// Opaque handle to a page.  Page 0-2 are reserved and never returned by `alloc`.
///
/// The const generic `PAGE_SIZE` is the page size in bytes that this handle
/// belongs to.  A `PageId<1024>` cannot be used with a `Pager<4096>` — the
/// compiler rejects the mismatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PageId<const PAGE_SIZE: usize>(pub u64);

/// A fixed-size view into one page of the memory map.
///
/// This is an unsized type (like `str` or `Path`); you always hold it behind a
/// reference (`&MappedPage` or `&mut MappedPage`).
///
/// # Invalidation
///
/// All `MappedPage` references are **invalidated** after any grow operation
/// because the entire file is remapped.  Do not hold references across calls
/// that may trigger a grow.
#[repr(transparent)]
pub struct MappedPage([u8]);

impl<const PAGE_SIZE: usize> PageId<PAGE_SIZE> {
    /// Borrow the page contents immutably.
    pub fn get<'a>(&self, pager: &'a Pager<PAGE_SIZE>) -> Result<&'a MappedPage, MappedPageError> {
        pager.get_page(*self)
    }

    /// Borrow the page contents mutably.
    pub fn get_mut<'a>(
        &self,
        pager: &'a mut Pager<PAGE_SIZE>,
    ) -> Result<&'a mut MappedPage, MappedPageError> {
        pager.get_page_mut(*self)
    }
}

impl MappedPage {
    /// View the raw bytes of this page.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Mutably view the raw bytes of this page.
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        &mut self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Construct a `&MappedPage` from a `&[u8]` slice of exactly one page.
    ///
    /// # Safety
    /// The slice must remain valid for lifetime `'a` and must not alias any
    /// concurrent mutable borrow.
    pub(crate) unsafe fn from_slice(s: &[u8]) -> &Self {
        // SAFETY: MappedPage is repr(transparent) over [u8].
        unsafe { &*(s as *const [u8] as *const MappedPage) }
    }

    /// Construct a `&mut MappedPage` from a `&mut [u8]` slice of exactly one page.
    ///
    /// # Safety
    /// The slice must remain valid and exclusively borrowed for lifetime `'a`.
    pub(crate) unsafe fn from_slice_mut(s: &mut [u8]) -> &mut Self {
        // SAFETY: MappedPage is repr(transparent) over [u8].
        unsafe { &mut *(s as *mut [u8] as *mut MappedPage) }
    }
}
