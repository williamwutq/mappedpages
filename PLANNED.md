# Planned Features for mappedpages

This document outlines planned enhancements to the `mappedpages` crate, a crash-consistent, memory-mapped, file-backed fixed-size page allocator.

## 1. Page Defragmentation

Add compaction functionality to reorganize pages and reclaim space:

- `fn defragment(&mut self) -> Result<(), MappedPageError>`
- Move allocated pages to eliminate gaps
- Update page references automatically
- Especially useful when combined with sub-page allocation

This would help maintain optimal file layout over time as pages are allocated and freed.

## 2. Backup and Snapshot Support

Implement utilities for creating point-in-time snapshots:

- `fn create_snapshot(&self, path: impl AsRef<Path>) -> Result<(), MappedPageError>`
- Copy-on-write snapshot mechanism
- Incremental backup capabilities
- Snapshot metadata tracking

This would enable robust backup strategies and point-in-time recovery.
