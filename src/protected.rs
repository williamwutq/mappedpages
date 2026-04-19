//! Protected pages: crash-consistent copy-on-write pages backed by two physical pages.

use crate::MappedPageError;
use crate::page::MappedPage;
use crate::pager::Pager;

/// Opaque handle to a protected (crash-consistent) page.
///
/// The `u64` is a 0-based logical slot index into the protected-page directory.
/// It is stable across reopens as long as the page is not freed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProtectedPageId(pub u64);

impl ProtectedPageId {
    /// Read the currently active copy of this protected page.
    pub fn get<'a>(&self, pager: &'a Pager) -> Result<&'a MappedPage, MappedPageError> {
        pager.get_protected_page(*self)
    }

    /// Obtain a writer for this protected page.
    ///
    /// The returned [`ProtectedPageWriter`] holds an exclusive borrow on the
    /// pager.  Writes are not visible to readers until [`ProtectedPageWriter::commit`]
    /// is called.
    pub fn get_mut<'a>(
        &self,
        pager: &'a mut Pager,
    ) -> Result<ProtectedPageWriter<'a>, MappedPageError> {
        pager.get_protected_page_mut(*self)
    }
}

/// An in-progress write to a protected page.
///
/// Obtain one via [`ProtectedPageId::get_mut`].  Call [`commit`](Self::commit)
/// to make the write durable and crash-consistent.  Dropping without committing
/// discards the write (the inactive physical page may contain partial data, but
/// the active page — visible to readers — is unchanged until commit).
pub struct ProtectedPageWriter<'a> {
    pub(crate) pager: &'a mut Pager,
    pub(crate) id: ProtectedPageId,
    /// Physical page number of the slot we are writing into (the inactive copy).
    pub(crate) inactive_phys_page: u64,
    /// Slot index (0 or 1) of the inactive copy, which becomes active after commit.
    pub(crate) inactive_slot: u8,
}

impl<'a> ProtectedPageWriter<'a> {
    /// Mutable view of the page being written.
    ///
    /// Changes are not visible to readers until [`commit`](Self::commit) is called.
    pub fn page_mut(&mut self) -> &mut MappedPage {
        let ps = self.pager.page_size;
        let off = self.inactive_phys_page as usize * ps;
        // SAFETY: mmap was verified available when the writer was created.
        // We hold &mut Pager, so no concurrent grow or mmap replacement can occur.
        let slice = &mut self
            .pager
            .mmap
            .as_mut()
            .expect("mmap available: verified at ProtectedPageWriter construction")[off..off + ps];
        unsafe { MappedPage::from_slice_mut(slice) }
    }

    /// Flush the written page and atomically flip the active-copy pointer.
    ///
    /// After this returns `Ok(())`, the written data is the new active copy and
    /// will survive a crash.
    pub fn commit(self) -> Result<(), MappedPageError> {
        self.pager
            .commit_protected_write(self.id, self.inactive_phys_page, self.inactive_slot)
    }
}
