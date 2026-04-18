//! Internal metadata structures: Superblock and MetaPage.
//!
//! On-disk Superblock layout (fixed 28 bytes; rest of page 0 unused):
//! ```text
//! [0..4]   "MPPG" tag
//! [4]      version major (compatibility check)
//! [5]      version minor (compatibility check)
//! [6]      version patch (informational; not checked on open)
//! [7]      version build (informational; not checked on open)
//! [8..12]  page_size_log2:  u32
//! [12]     active_meta:     u8   (0 = A = page 1, 1 = B = page 2)
//! [13..16] _pad:            [u8; 3]
//! [16..20] meta_checksum:   u32  (CRC32 of the full active metadata page)
//! ```
//!
//! A file is compatible if its tag is `"MPPG"` and major+minor match; patch
//! and build may differ (all 0.1.x.x files are mutually compatible).
//!
//! On-disk MetaPage layout (fills one page):
//! ```text
//! [0..8]          total_pages:  u64
//! [8..16]         generation:   u64
//! [16..24]        free_count:   u64
//! [24..page_size-4] bitmap:     [u8]  (1 bit per page; bit=0 means free)
//! [page_size-4..]   checksum:   u32  (CRC32 of bytes [0..page_size-4])
//! ```

pub(crate) const MAGIC_TAG: &[u8; 4] = b"MPPG";
pub(crate) const VERSION_MAJOR: u8 = 0;
pub(crate) const VERSION_MINOR: u8 = 1;
pub(crate) const VERSION_PATCH: u8 = 0;
pub(crate) const VERSION_BUILD: u8 = 0;
/// Full magic for files written by this build: `"MPPG"` + `[major, minor, patch, build]`.
pub(crate) const MAGIC: u64 = u64::from_le_bytes([
    b'M', b'P', b'P', b'G',
    VERSION_MAJOR, VERSION_MINOR, VERSION_PATCH, VERSION_BUILD,
]);
/// First page index that may be returned to callers (pages 0-2 are reserved).
pub(crate) const FIRST_DATA_PAGE: u64 = 3;
/// Minimum allowed `page_size_log2` (2^10 = 1024 bytes).
pub(crate) const MIN_PAGE_SIZE_LOG2: u32 = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MetaSelector {
    A,
    B,
}

impl MetaSelector {
    pub(crate) fn page_id(self) -> u64 {
        match self {
            MetaSelector::A => 1,
            MetaSelector::B => 2,
        }
    }

    pub(crate) fn other(self) -> Self {
        match self {
            MetaSelector::A => MetaSelector::B,
            MetaSelector::B => MetaSelector::A,
        }
    }

    fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(MetaSelector::A),
            1 => Some(MetaSelector::B),
            _ => None,
        }
    }

    fn to_byte(self) -> u8 {
        match self {
            MetaSelector::A => 0,
            MetaSelector::B => 1,
        }
    }
}

/// Parsed view of page 0.
pub(crate) struct Superblock {
    /// Full 8-byte magic: `"MPPG"` tag + `[major, minor, patch, build]`.
    pub magic: u64,
    pub page_size_log2: u32,
    pub active_meta: MetaSelector,
    /// CRC32 of the full active metadata page bytes.
    pub meta_checksum: u32,
}

/// In-memory representation of an A or B metadata page.
pub(crate) struct MetaPage {
    pub total_pages: u64,
    pub generation: u64,
    pub free_count: u64,
    /// One bit per page: 0 = free, 1 = allocated.  Length = ceil(total_pages / 8).
    pub bitmap: Vec<u8>,
}

// ── Superblock ────────────────────────────────────────────────────────────────

impl Superblock {
    pub(crate) fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < 20 {
            return None;
        }
        let magic = u64::from_le_bytes(data[0..8].try_into().ok()?);
        let page_size_log2 = u32::from_le_bytes(data[8..12].try_into().ok()?);
        let active_meta = MetaSelector::from_byte(data[12])?;
        let meta_checksum = u32::from_le_bytes(data[16..20].try_into().ok()?);
        Some(Superblock { magic, page_size_log2, active_meta, meta_checksum })
    }

    pub(crate) fn write_to(&self, buf: &mut [u8]) {
        buf[0..8].copy_from_slice(&self.magic.to_le_bytes());
        buf[8..12].copy_from_slice(&self.page_size_log2.to_le_bytes());
        buf[12] = self.active_meta.to_byte();
        buf[13..16].fill(0);
        buf[16..20].copy_from_slice(&self.meta_checksum.to_le_bytes());
    }

    /// Returns true if this file was written by a compatible version (same tag, major, minor).
    pub(crate) fn is_valid(&self) -> bool {
        let b = self.magic.to_le_bytes();
        &b[0..4] == MAGIC_TAG && b[4] == VERSION_MAJOR && b[5] == VERSION_MINOR
    }
}

// ── MetaPage ──────────────────────────────────────────────────────────────────

impl MetaPage {
    /// Deserialize from a raw page slice.  Returns `None` if the checksum fails.
    pub(crate) fn from_bytes(data: &[u8]) -> Option<Self> {
        let page_size = data.len();
        if page_size < 28 {
            return None;
        }
        let stored_csum = u32::from_le_bytes(data[page_size - 4..].try_into().ok()?);
        let computed_csum = crc32fast::hash(&data[..page_size - 4]);
        if stored_csum != computed_csum {
            return None;
        }
        let total_pages = u64::from_le_bytes(data[0..8].try_into().ok()?);
        let generation = u64::from_le_bytes(data[8..16].try_into().ok()?);
        let free_count = u64::from_le_bytes(data[16..24].try_into().ok()?);
        let bitmap_len = (total_pages as usize).div_ceil(8);
        let bitmap_end = 24 + bitmap_len;
        if bitmap_end > page_size - 4 {
            return None;
        }
        let bitmap = data[24..bitmap_end].to_vec();
        Some(MetaPage { total_pages, generation, free_count, bitmap })
    }

    /// Serialize into a raw page slice.
    pub(crate) fn write_to(&self, buf: &mut [u8]) {
        let page_size = buf.len();
        buf[0..8].copy_from_slice(&self.total_pages.to_le_bytes());
        buf[8..16].copy_from_slice(&self.generation.to_le_bytes());
        buf[16..24].copy_from_slice(&self.free_count.to_le_bytes());
        let bitmap_len = self.bitmap.len();
        buf[24..24 + bitmap_len].copy_from_slice(&self.bitmap);
        // zero padding between bitmap and checksum
        buf[24 + bitmap_len..page_size - 4].fill(0);
        let csum = crc32fast::hash(&buf[..page_size - 4]);
        buf[page_size - 4..].copy_from_slice(&csum.to_le_bytes());
    }

    /// Compute the CRC32 of a raw metadata page (used to populate the superblock).
    pub(crate) fn page_checksum(data: &[u8]) -> u32 {
        crc32fast::hash(data)
    }

    /// Find a free page (bit = 0), mark it allocated, and return its page index.
    /// Returns `None` if no free page exists.
    pub(crate) fn alloc_page(&mut self) -> Option<u64> {
        for (byte_idx, byte) in self.bitmap.iter_mut().enumerate() {
            if *byte != 0xff {
                let bit = byte.trailing_ones();
                let page_id = byte_idx as u64 * 8 + bit as u64;
                // Check bounds before modifying: trailing bits beyond total_pages
                // are kept zero but are not valid pages.
                if page_id >= self.total_pages {
                    break;
                }
                *byte |= 1 << bit;
                self.free_count = self.free_count.saturating_sub(1);
                return Some(page_id);
            }
        }
        None
    }

    /// Mark page `id` as free (bit = 0).  Returns `false` if already free or out of range.
    pub(crate) fn free_page(&mut self, id: u64) -> bool {
        if id >= self.total_pages {
            return false;
        }
        let byte_idx = (id / 8) as usize;
        let bit = (id % 8) as u8;
        if self.bitmap[byte_idx] & (1 << bit) == 0 {
            return false; // already free
        }
        self.bitmap[byte_idx] &= !(1 << bit);
        self.free_count += 1;
        true
    }

    /// Build a fresh MetaPage for a newly created file with the given total page count.
    /// Pages 0-2 are pre-marked as allocated.
    pub(crate) fn new_for_capacity(total_pages: u64) -> Self {
        let bitmap_len = (total_pages as usize).div_ceil(8);
        let mut bitmap = vec![0u8; bitmap_len];
        // mark pages 0, 1, 2 as allocated
        for reserved in 0..FIRST_DATA_PAGE.min(total_pages) {
            bitmap[(reserved / 8) as usize] |= 1 << (reserved % 8);
        }
        let free_count = total_pages.saturating_sub(FIRST_DATA_PAGE);
        MetaPage { total_pages, generation: 0, free_count, bitmap }
    }

    /// Grow this metadata to cover `new_total_pages`, extending the bitmap.
    pub(crate) fn grow_to(&mut self, new_total_pages: u64) {
        let new_bitmap_len = (new_total_pages as usize).div_ceil(8);
        self.bitmap.resize(new_bitmap_len, 0);
        let added = new_total_pages.saturating_sub(self.total_pages);
        self.free_count += added;
        self.total_pages = new_total_pages;
    }
}
