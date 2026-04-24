//! Thread-safe access to [`Pager`] via a reader-writer lock.
//!
//! [`ConcurrentPager<PAGE_SIZE>`] wraps a [`Pager`] in an `Arc<RwLock<…>>`
//! so that multiple threads can hold read access simultaneously while
//! allocation, deallocation, and mutation remain exclusive.
//!
//! # Reader-writer semantics
//!
//! - **Concurrent reads** — call [`read`](ConcurrentPager::read) to obtain a
//!   [`PagerReadGuard`].  Multiple threads can each hold a read guard at the
//!   same time, accessing different pages in parallel without blocking one another.
//! - **Exclusive writes** — call [`write`](ConcurrentPager::write) to obtain a
//!   [`PagerWriteGuard`], which gives `&mut Pager` access for allocation,
//!   deallocation, and page mutation.  No reader or other writer may hold a lock
//!   while a write guard is active.
//!
//! Both guards dereference to the inner [`Pager`], so the entire existing `Pager`
//! API is usable through them without any adapter methods.
//!
//! # Sharing across threads
//!
//! `ConcurrentPager` is `Clone`; all clones share the same underlying [`Pager`].
//! It is `Send + Sync`, so it can be moved into threads or wrapped in an `Arc`
//! for multi-producer sharing.
//!
//! # Example
//!
//! ```rust,no_run
//! use mappedpages::{Pager, ConcurrentPager};
//! use std::thread;
//!
//! let pager = Pager::<4096>::create("data.mpp").unwrap();
//! let shared = ConcurrentPager::new(pager);
//!
//! // Allocate a page on the main thread.
//! let page_id = shared.write().unwrap().alloc().unwrap();
//!
//! // Read from a clone on a worker thread.
//! let reader = shared.clone();
//! thread::spawn(move || {
//!     let guard = reader.read().unwrap();
//!     let _page = page_id.get(&*guard).unwrap();
//! }).join().unwrap();
//! ```

use std::fmt;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError};

use crate::pager::Pager;

/// Error returned by [`ConcurrentPager`] locking operations.
#[derive(Debug)]
pub enum ConcurrentPagerError {
    /// A thread panicked while holding the lock, leaving it poisoned.
    Poisoned,
    /// The lock is currently held; `try_read` / `try_write` would block.
    WouldBlock,
}

impl fmt::Display for ConcurrentPagerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConcurrentPagerError::Poisoned => write!(
                f,
                "pager lock is poisoned: a thread panicked while holding it"
            ),
            ConcurrentPagerError::WouldBlock => write!(
                f,
                "pager lock is currently held; try_read/try_write would block"
            ),
        }
    }
}

impl std::error::Error for ConcurrentPagerError {}

impl<T> From<PoisonError<T>> for ConcurrentPagerError {
    fn from(_: PoisonError<T>) -> Self {
        ConcurrentPagerError::Poisoned
    }
}

/// A thread-safe, reference-counted wrapper around [`Pager`].
///
/// See the [module documentation](self) for usage details and examples.
pub struct ConcurrentPager<const PAGE_SIZE: usize> {
    inner: Arc<RwLock<Pager<PAGE_SIZE>>>,
}

impl<const PAGE_SIZE: usize> ConcurrentPager<PAGE_SIZE> {
    /// Wrap `pager` in a thread-safe, reference-counted reader-writer lock.
    pub fn new(pager: Pager<PAGE_SIZE>) -> Self {
        ConcurrentPager {
            inner: Arc::new(RwLock::new(pager)),
        }
    }

    /// Acquire a shared read lock, blocking until no writer holds the lock.
    ///
    /// Multiple threads can hold read guards simultaneously.
    /// Returns [`ConcurrentPagerError::Poisoned`] if a thread previously
    /// panicked while holding the write lock.
    pub fn read(&self) -> Result<PagerReadGuard<'_, PAGE_SIZE>, ConcurrentPagerError> {
        Ok(PagerReadGuard {
            guard: self.inner.read()?,
        })
    }

    /// Acquire an exclusive write lock, blocking until all other locks are released.
    ///
    /// Returns [`ConcurrentPagerError::Poisoned`] if the lock is poisoned.
    pub fn write(&self) -> Result<PagerWriteGuard<'_, PAGE_SIZE>, ConcurrentPagerError> {
        Ok(PagerWriteGuard {
            guard: self.inner.write()?,
        })
    }

    /// Try to acquire a shared read lock without blocking.
    ///
    /// Returns [`ConcurrentPagerError::WouldBlock`] immediately if a writer
    /// holds the lock.
    pub fn try_read(&self) -> Result<PagerReadGuard<'_, PAGE_SIZE>, ConcurrentPagerError> {
        match self.inner.try_read() {
            Ok(guard) => Ok(PagerReadGuard { guard }),
            Err(TryLockError::Poisoned(e)) => Err(PoisonError::new(e.into_inner()).into()),
            Err(TryLockError::WouldBlock) => Err(ConcurrentPagerError::WouldBlock),
        }
    }

    /// Try to acquire an exclusive write lock without blocking.
    ///
    /// Returns [`ConcurrentPagerError::WouldBlock`] immediately if any reader
    /// or writer holds the lock.
    pub fn try_write(&self) -> Result<PagerWriteGuard<'_, PAGE_SIZE>, ConcurrentPagerError> {
        match self.inner.try_write() {
            Ok(guard) => Ok(PagerWriteGuard { guard }),
            Err(TryLockError::Poisoned(e)) => Err(PoisonError::new(e.into_inner()).into()),
            Err(TryLockError::WouldBlock) => Err(ConcurrentPagerError::WouldBlock),
        }
    }

    /// Consume this wrapper and return the inner [`Pager`] if no other clones exist.
    ///
    /// Returns `None` if other `ConcurrentPager` clones still point to the same pager.
    /// If the lock is poisoned the inner value is recovered regardless.
    pub fn into_inner(self) -> Option<Pager<PAGE_SIZE>> {
        Arc::try_unwrap(self.inner)
            .ok()
            .map(|lock| lock.into_inner().unwrap_or_else(|e| e.into_inner()))
    }
}

impl<const PAGE_SIZE: usize> Clone for ConcurrentPager<PAGE_SIZE> {
    fn clone(&self) -> Self {
        ConcurrentPager {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<const PAGE_SIZE: usize> From<Pager<PAGE_SIZE>> for ConcurrentPager<PAGE_SIZE> {
    fn from(pager: Pager<PAGE_SIZE>) -> Self {
        ConcurrentPager::new(pager)
    }
}

/// A shared read guard for a [`ConcurrentPager`].
///
/// Derefs to `&Pager<PAGE_SIZE>`, exposing all read-only [`Pager`] methods
/// (e.g. [`PageId::get`](crate::PageId::get), [`Pager::page_count`],
/// [`Pager::iter_allocated_pages`]).
///
/// Multiple `PagerReadGuard`s can be held simultaneously across threads.
/// The read lock is released when this guard is dropped.
pub struct PagerReadGuard<'a, const PAGE_SIZE: usize> {
    guard: RwLockReadGuard<'a, Pager<PAGE_SIZE>>,
}

impl<'a, const PAGE_SIZE: usize> Deref for PagerReadGuard<'a, PAGE_SIZE> {
    type Target = Pager<PAGE_SIZE>;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

/// An exclusive write guard for a [`ConcurrentPager`].
///
/// Derefs to `&mut Pager<PAGE_SIZE>`, exposing all [`Pager`] methods including
/// `alloc`, `free`, `alloc_protected`, and mutable page access via
/// [`PageId::get_mut`](crate::PageId::get_mut) and
/// [`ProtectedPageId::get_mut`](crate::ProtectedPageId::get_mut).
///
/// Only one `PagerWriteGuard` can exist at a time; no read guards may coexist.
/// The write lock is released when this guard is dropped.
pub struct PagerWriteGuard<'a, const PAGE_SIZE: usize> {
    guard: RwLockWriteGuard<'a, Pager<PAGE_SIZE>>,
}

impl<'a, const PAGE_SIZE: usize> Deref for PagerWriteGuard<'a, PAGE_SIZE> {
    type Target = Pager<PAGE_SIZE>;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl<'a, const PAGE_SIZE: usize> DerefMut for PagerWriteGuard<'a, PAGE_SIZE> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}
