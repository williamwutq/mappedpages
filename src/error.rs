use std::fmt;
use std::io;

#[derive(Debug)]
pub enum MappedPageError {
    /// Underlying I/O failure.
    Io(io::Error),
    /// `page_size_log2` is below `MIN_PAGE_SIZE_LOG2` (10).
    InvalidPageSize,
    /// Page 0 (superblock) has an unrecognised magic, incompatible version, or
    /// the file is too small to contain the minimum four pages.
    CorruptSuperblock,
    /// Both metadata pages (A and B) failed their checksums; the file cannot be
    /// recovered automatically.
    CorruptMetadata,
    /// The `PageId` is ≥ `page_count()` or refers to a reserved page (0–2).
    OutOfBounds,
    /// The `PageId` refers to a reserved internal page (0–2).
    ReservedPage,
    /// Attempted to free a page that is already marked free.
    DoubleFree,
    /// The pager's memory mapping is no longer available (failed remap after a
    /// grow error).  The pager cannot be used further; reopen the file.
    Unavailable,
    /// Both A and B copies of a protected-page directory block failed their
    /// checksums; the directory cannot be recovered automatically.
    CorruptProtectedDirectory,
    /// The directory block array in page 0 is corrupt (bad checksum or overflows).
    CorruptDirectoryIndex,
    /// No room remains in page 0 for additional directory block pair references.
    DirectoryFull,
}

impl fmt::Display for MappedPageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MappedPageError::Io(e) => write!(f, "I/O error: {e}"),
            MappedPageError::InvalidPageSize => {
                write!(f, "page_size_log2 must be ≥ 10 (1024 bytes)")
            }
            MappedPageError::CorruptSuperblock => {
                write!(f, "superblock is corrupt or from an incompatible version")
            }
            MappedPageError::CorruptMetadata => {
                write!(f, "both metadata pages are corrupt; cannot recover")
            }
            MappedPageError::OutOfBounds => write!(f, "page id is out of range"),
            MappedPageError::ReservedPage => {
                write!(f, "page id refers to a reserved page (0–2)")
            }
            MappedPageError::DoubleFree => write!(f, "page is already free"),
            MappedPageError::Unavailable => {
                write!(f, "pager memory mapping unavailable; reopen the file")
            }
            MappedPageError::CorruptProtectedDirectory => {
                write!(f, "both directory page copies are corrupt; cannot recover")
            }
            MappedPageError::CorruptDirectoryIndex => {
                write!(f, "directory block index in page 0 is corrupt")
            }
            MappedPageError::DirectoryFull => {
                write!(f, "page 0 has no room for additional directory block references")
            }
        }
    }
}

impl std::error::Error for MappedPageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MappedPageError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for MappedPageError {
    fn from(e: io::Error) -> Self {
        MappedPageError::Io(e)
    }
}
