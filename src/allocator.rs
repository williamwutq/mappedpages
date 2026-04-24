//! [`PageAllocator`] trait and its [`PageHandle`] bound.

use crate::error::MappedPageError;
use crate::page::{MappedPage, PageId};
use crate::pager::Pager;
use crate::protected::{ProtectedPageId, ProtectedPageWriter};

/// A handle to an allocated page that can borrow its contents from the
/// allocator that produced it.
///
/// The `Mut` associated type allows each handle to name its own mutable
/// accessor: [`PageId`] returns `&mut MappedPage` directly, while
/// [`ProtectedPageId`] returns a [`ProtectedPageWriter`] that must be
/// committed to make the write crash-consistent.
///
/// The `Error` associated type is the error returned by [`get`](Self::get)
/// and [`get_mut`](Self::get_mut), allowing implementations to use error
/// types other than [`MappedPageError`].
pub trait PageHandle<A: ?Sized> {
    /// The error type returned by [`get`](Self::get) and [`get_mut`](Self::get_mut).
    type Error;

    /// The type returned by [`get_mut`](Self::get_mut).
    type Mut<'a>: 'a
    where
        A: 'a,
        Self: 'a;

    /// Immutably borrow the page contents from `allocator`.
    fn get<'a>(&self, allocator: &'a A) -> Result<&'a MappedPage, Self::Error>;

    /// Mutably access the page contents through `allocator`.
    fn get_mut<'a>(&self, allocator: &'a mut A) -> Result<Self::Mut<'a>, Self::Error>;
}

/// Allocates and frees pages of type `AllocatedType`.
///
/// `AllocatedType` must implement [`PageHandle<Self>`] so that callers can
/// borrow page data back through the same allocator.
///
/// The `Error` associated type is the error returned by [`alloc`](Self::alloc)
/// and [`free`](Self::free), allowing implementations to use error types other
/// than [`MappedPageError`].
pub trait PageAllocator<AllocatedType: PageHandle<Self>>: Sized {
    /// The error type returned by [`alloc`](Self::alloc) and [`free`](Self::free).
    type Error;

    /// Allocate a fresh page, returning a handle to it.
    fn alloc(&mut self) -> Result<AllocatedType, Self::Error>;

    /// Free a previously allocated page.
    fn free(&mut self, id: AllocatedType) -> Result<(), Self::Error>;
}

// ── PageId ────────────────────────────────────────────────────────────────────

impl<const PAGE_SIZE: usize> PageHandle<Pager<PAGE_SIZE>> for PageId<PAGE_SIZE> {
    type Error = MappedPageError;
    type Mut<'a> = &'a mut MappedPage;

    fn get<'a>(&self, pager: &'a Pager<PAGE_SIZE>) -> Result<&'a MappedPage, MappedPageError> {
        pager.get_page(*self)
    }

    fn get_mut<'a>(
        &self,
        pager: &'a mut Pager<PAGE_SIZE>,
    ) -> Result<&'a mut MappedPage, MappedPageError> {
        pager.get_page_mut(*self)
    }
}

impl<const PAGE_SIZE: usize> PageAllocator<PageId<PAGE_SIZE>> for Pager<PAGE_SIZE> {
    type Error = MappedPageError;

    fn alloc(&mut self) -> Result<PageId<PAGE_SIZE>, MappedPageError> {
        Pager::alloc(self)
    }

    fn free(&mut self, id: PageId<PAGE_SIZE>) -> Result<(), MappedPageError> {
        Pager::free(self, id)
    }
}

// ── ProtectedPageId ───────────────────────────────────────────────────────────

impl<const PAGE_SIZE: usize> PageHandle<Pager<PAGE_SIZE>> for ProtectedPageId<PAGE_SIZE> {
    type Error = MappedPageError;
    /// A [`ProtectedPageWriter`] that must be [`commit`](ProtectedPageWriter::commit)ted
    /// to make the write crash-consistent.
    type Mut<'a> = ProtectedPageWriter<'a, PAGE_SIZE>;

    fn get<'a>(&self, pager: &'a Pager<PAGE_SIZE>) -> Result<&'a MappedPage, MappedPageError> {
        pager.get_protected_page(*self)
    }

    fn get_mut<'a>(
        &self,
        pager: &'a mut Pager<PAGE_SIZE>,
    ) -> Result<ProtectedPageWriter<'a, PAGE_SIZE>, MappedPageError> {
        pager.get_protected_page_mut(*self)
    }
}

impl<const PAGE_SIZE: usize> PageAllocator<ProtectedPageId<PAGE_SIZE>> for Pager<PAGE_SIZE> {
    type Error = MappedPageError;

    fn alloc(&mut self) -> Result<ProtectedPageId<PAGE_SIZE>, MappedPageError> {
        Pager::alloc_protected(self)
    }

    fn free(&mut self, id: ProtectedPageId<PAGE_SIZE>) -> Result<(), MappedPageError> {
        Pager::free_protected(self, id)
    }
}
