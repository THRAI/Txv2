#![no_std]

extern crate alloc;

pub mod capability;
pub mod journal;
mod journal_replay;
pub mod mapping;
pub mod mutation;
pub mod ondisk;
pub mod pager;

pub use journal_replay::{JournalReplayReport, clean_replayed_journal, replay_journal};

pub type Result<T> = core::result::Result<T, Ext4FormatError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ext4FormatError {
    BadMagic,
    Corrupt,
    OutOfBounds,
    Truncated,
    Unsupported,
    WouldBlock,
}
