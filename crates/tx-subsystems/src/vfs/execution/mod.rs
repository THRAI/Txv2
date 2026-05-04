//! VFS execution surface placeholder.
//!
//! The previous skeleton's read/create/ext4 test harness lived here.
//! Per the spec-reconciliation plan
//! (`docs/progress/decisions/2026-05-04-vm-tty-subsystems-move-deferred.md`)
//! the skeleton's byte-buffer Frame model is being removed. The harness
//! relied on byte-buffer Frame and is therefore deleted; tx-ext4 tests
//! that depended on `read_harness::VfsReadHarness` will be ported or
//! removed in Phase 3.
