//! tx-fat-format: FAT12/16/32 on-disk structures and pager.
//!
//! #![no_std] — depends only on `alloc` for vector and box.
//!
//! This crate is the format/pager half of the tx-fat filesystem backend.
//! It defines the `BlockImage` trait (read/write 512-byte logical blocks),
//! the on-disk BPB / directory-entry / LFN-entry types, and a `FatPager<I: BlockImage>`
//! that performs cluster-chain walking, directory enumeration, and file reads.

#![no_std]

extern crate alloc;

pub mod ondisk;
pub mod pager;

// Re-export the pager types consumers need.
pub use ondisk::{BPBParseError, FatType};
pub use pager::{BlockImage, DirEntryLite, FatFormatError, FatPager, Page4K, Result, BLOCK_SIZE, MAX_LFN_LENGTH};
