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
    b'M',
    b'P',
    b'P',
    b'G',
    VERSION_MAJOR,
    VERSION_MINOR,
    VERSION_PATCH,
    VERSION_BUILD,
]);
/// First page index that may be returned to callers (pages 0-2 are reserved).
pub(crate) const FIRST_DATA_PAGE: u64 = 3;

/// Byte offset in page 0 where the directory block array begins (after the 20-byte superblock).
pub(crate) const PAGE0_DIR_OFFSET: usize = 20;
/// Bytes per directory block reference stored in page 0: page_a(8) + page_b(8) + active(1).
pub(crate) const DIR_BLOCK_REF_SIZE: usize = 17;
/// Bytes per protected-page entry in a directory page: in_use(1)+page_a(8)+page_b(8)+slot(1)+gen(8)+csum(4).
pub(crate) const DIR_ENTRY_SIZE: usize = 30;

/// Maximum number of directory block pairs whose references fit in page 0.
/// Layout: [20..24] = count(4), [24..ps-4] = entries, [ps-4..ps] = checksum(4).
pub(crate) fn max_dir_blocks(page_size: usize) -> usize {
    page_size.saturating_sub(PAGE0_DIR_OFFSET + 4 + 4) / DIR_BLOCK_REF_SIZE
}

/// Maximum number of protected-page entries that fit in one directory page.
/// Layout: [0..8] = header(8), [8..ps-4] = entries, [ps-4..ps] = checksum(4).
pub(crate) fn dir_entries_per_page(page_size: usize) -> usize {
    page_size.saturating_sub(8 + 4) / DIR_ENTRY_SIZE
}

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
        Some(Superblock {
            magic,
            page_size_log2,
            active_meta,
            meta_checksum,
        })
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
        Some(MetaPage {
            total_pages,
            generation,
            free_count,
            bitmap,
        })
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
        MetaPage {
            total_pages,
            generation: 0,
            free_count,
            bitmap,
        }
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

// ── Directory block references (stored in page 0) ─────────────────────────────

/// Reference to one A/B pair of directory pages, stored in page 0's extended section.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DirBlockRef {
    pub page_a: u64,
    pub page_b: u64,
    pub active: MetaSelector,
}

impl DirBlockRef {
    pub(crate) fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < DIR_BLOCK_REF_SIZE {
            return None;
        }
        let page_a = u64::from_le_bytes(data[0..8].try_into().ok()?);
        let page_b = u64::from_le_bytes(data[8..16].try_into().ok()?);
        let active = MetaSelector::from_byte(data[16])?;
        Some(DirBlockRef {
            page_a,
            page_b,
            active,
        })
    }

    pub(crate) fn write_to(&self, buf: &mut [u8]) {
        buf[0..8].copy_from_slice(&self.page_a.to_le_bytes());
        buf[8..16].copy_from_slice(&self.page_b.to_le_bytes());
        buf[16] = self.active.to_byte();
    }
}

/// Parse directory block references from page 0's extended section.
///
/// Returns `Ok(vec![])` when there are no directory blocks (backward-compatible
/// with files created before protected pages were added).  Returns `Err(())`
/// only if the count is non-zero but the checksum fails or a ref is malformed.
pub(crate) fn read_dir_blocks(page0: &[u8]) -> Result<Vec<DirBlockRef>, ()> {
    let page_size = page0.len();
    if page_size < PAGE0_DIR_OFFSET + 8 {
        return Ok(vec![]);
    }
    let count = u32::from_le_bytes(
        page0[PAGE0_DIR_OFFSET..PAGE0_DIR_OFFSET + 4]
            .try_into()
            .map_err(|_| ())?,
    ) as usize;
    if count == 0 {
        return Ok(vec![]);
    }
    // Validate the dir section checksum: covers [PAGE0_DIR_OFFSET..page_size-4].
    if page_size < PAGE0_DIR_OFFSET + 4 + 4 {
        return Err(());
    }
    let stored_csum = u32::from_le_bytes(page0[page_size - 4..].try_into().map_err(|_| ())?);
    let computed_csum = crc32fast::hash(&page0[PAGE0_DIR_OFFSET..page_size - 4]);
    if stored_csum != computed_csum {
        return Err(());
    }
    if count > max_dir_blocks(page_size) {
        return Err(());
    }
    let entries_start = PAGE0_DIR_OFFSET + 4;
    let mut blocks = Vec::with_capacity(count);
    for i in 0..count {
        let off = entries_start + i * DIR_BLOCK_REF_SIZE;
        let block = DirBlockRef::from_bytes(&page0[off..off + DIR_BLOCK_REF_SIZE]).ok_or(())?;
        blocks.push(block);
    }
    Ok(blocks)
}

/// Serialize directory block references into page 0's extended section.
///
/// Writes `[PAGE0_DIR_OFFSET..page_size]`: count(4) + entries + padding + checksum(4).
pub(crate) fn write_dir_blocks(dir_blocks: &[DirBlockRef], page0: &mut [u8]) {
    let page_size = page0.len();
    let count = dir_blocks.len() as u32;
    page0[PAGE0_DIR_OFFSET..PAGE0_DIR_OFFSET + 4].copy_from_slice(&count.to_le_bytes());
    let entries_start = PAGE0_DIR_OFFSET + 4;
    for (i, block) in dir_blocks.iter().enumerate() {
        let off = entries_start + i * DIR_BLOCK_REF_SIZE;
        block.write_to(&mut page0[off..off + DIR_BLOCK_REF_SIZE]);
    }
    let entries_end = entries_start + dir_blocks.len() * DIR_BLOCK_REF_SIZE;
    page0[entries_end..page_size - 4].fill(0);
    let csum = crc32fast::hash(&page0[PAGE0_DIR_OFFSET..page_size - 4]);
    page0[page_size - 4..].copy_from_slice(&csum.to_le_bytes());
}

// ── Directory page entries ────────────────────────────────────────────────────

/// One entry in a protected-page directory page.
#[derive(Clone, Debug, Default)]
pub(crate) struct DirEntry {
    pub in_use: bool,
    pub page_a: u64,
    pub page_b: u64,
    /// Which physical page (0 = page_a, 1 = page_b) is the currently active/readable copy.
    pub active_slot: u8,
    pub generation: u64,
    /// CRC32 of the active physical page's content at the time of the last commit.
    pub checksum: u32,
}

impl DirEntry {
    pub(crate) fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < DIR_ENTRY_SIZE {
            return None;
        }
        let in_use = data[0] != 0;
        let page_a = u64::from_le_bytes(data[1..9].try_into().ok()?);
        let page_b = u64::from_le_bytes(data[9..17].try_into().ok()?);
        let active_slot = data[17];
        let generation = u64::from_le_bytes(data[18..26].try_into().ok()?);
        let checksum = u32::from_le_bytes(data[26..30].try_into().ok()?);
        Some(DirEntry {
            in_use,
            page_a,
            page_b,
            active_slot,
            generation,
            checksum,
        })
    }

    pub(crate) fn write_to(&self, buf: &mut [u8]) {
        buf[0] = self.in_use as u8;
        buf[1..9].copy_from_slice(&self.page_a.to_le_bytes());
        buf[9..17].copy_from_slice(&self.page_b.to_le_bytes());
        buf[17] = self.active_slot;
        buf[18..26].copy_from_slice(&self.generation.to_le_bytes());
        buf[26..30].copy_from_slice(&self.checksum.to_le_bytes());
    }
}

/// In-memory view of one directory page (A or B copy).
pub(crate) struct DirPage {
    /// Fixed slot count: determined by page size at creation, stored in the page header.
    pub capacity: u32,
    pub entries: Vec<DirEntry>,
}

impl DirPage {
    /// Create a fresh, all-free directory page for the given page size.
    pub(crate) fn new_empty(page_size: usize) -> Self {
        let capacity = dir_entries_per_page(page_size) as u32;
        let entries = (0..capacity as usize)
            .map(|_| DirEntry::default())
            .collect();
        DirPage { capacity, entries }
    }

    /// Deserialize and checksum-validate from a raw page slice.
    pub(crate) fn from_bytes(data: &[u8]) -> Option<Self> {
        let page_size = data.len();
        if page_size < 12 {
            return None;
        }
        let stored_csum = u32::from_le_bytes(data[page_size - 4..].try_into().ok()?);
        let computed_csum = crc32fast::hash(&data[..page_size - 4]);
        if stored_csum != computed_csum {
            return None;
        }
        let capacity = u32::from_le_bytes(data[0..4].try_into().ok()?) as usize;
        let entries_end = 8 + capacity * DIR_ENTRY_SIZE;
        if entries_end > page_size - 4 {
            return None;
        }
        let mut entries = Vec::with_capacity(capacity);
        for i in 0..capacity {
            let off = 8 + i * DIR_ENTRY_SIZE;
            entries.push(DirEntry::from_bytes(&data[off..off + DIR_ENTRY_SIZE])?);
        }
        Some(DirPage {
            capacity: capacity as u32,
            entries,
        })
    }

    /// Serialize into a raw page slice.
    pub(crate) fn write_to(&self, buf: &mut [u8]) {
        let page_size = buf.len();
        buf[0..4].copy_from_slice(&self.capacity.to_le_bytes());
        buf[4..8].fill(0);
        for (i, entry) in self.entries.iter().enumerate() {
            let off = 8 + i * DIR_ENTRY_SIZE;
            entry.write_to(&mut buf[off..off + DIR_ENTRY_SIZE]);
        }
        let entries_end = 8 + self.entries.len() * DIR_ENTRY_SIZE;
        buf[entries_end..page_size - 4].fill(0);
        let csum = crc32fast::hash(&buf[..page_size - 4]);
        buf[page_size - 4..].copy_from_slice(&csum.to_le_bytes());
    }
}
