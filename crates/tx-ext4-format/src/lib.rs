#![no_std]

extern crate alloc;

pub mod journal;
mod journal_replay;
pub mod mapping;
pub mod mutation;
pub mod ondisk;
pub mod pager;

pub use journal_replay::{clean_replayed_journal, replay_journal, JournalReplayReport};

pub type Result<T> = core::result::Result<T, Ext4FormatError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ext4FormatError {
    BadMagic,
    Corrupt,
    OutOfBounds,
    Truncated,
    Unsupported,
    /// The extent tree cannot grow any deeper while inserting a new
    /// logical-to-physical mapping.  Keep the insertion context in the
    /// error so callers and tests do not have to infer an opaque ENOSYS.
    ExtentTreeFull {
        inode: u32,
        logical_block: u32,
        depth: u16,
        entries: u16,
    },
    WouldBlock,
    NotEmpty,
    IsDirectory,
    NotDirectory,
    InvalidInput,
}
