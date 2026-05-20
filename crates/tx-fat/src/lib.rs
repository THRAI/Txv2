//! tx-fat: FAT12/16/32 kernel adapter.
//!
//! Implements `FsOps` and `FsPageBacking` for a FAT volume backed by a
//! `tx-fat-format::BlockImage`. Follows the tx-ext4 pattern:
//! `FatFsInstance<I>` wraps a `FatPager<I>` with spin-locking, mount-pin
//! registration, and a read-only flag.

#![no_std]

extern crate alloc;

pub mod adapter;
pub mod mount;
pub mod namespace;
mod pager;
mod read_backend;
