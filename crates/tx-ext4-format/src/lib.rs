#![no_std]

extern crate alloc;

pub mod capability;
pub mod journal;
mod journal_replay;
pub mod mapping;
pub mod mutation;
pub mod ondisk;
pub mod pager;

pub use journal_replay::{
    clean_replayed_journal, diagnose_recovery_preflight,
    diagnose_recovery_preflight_linux_uuid_semantics, preflight_recovery, recover_if_required,
    replay_journal, JournalPreflightError, JournalPreflightUnsupported, JournalReplayReport,
    RecoveryReport,
};

pub type Result<T> = core::result::Result<T, Ext4FormatError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ext4FormatError {
    BadMagic,
    Corrupt,
    OutOfBounds,
    Truncated,
    Unsupported,
    /// The extent tree cannot grow any deeper while inserting a new
    /// logical-to-physical mapping.
    ExtentTreeFull {
        inode: u32,
        logical_block: u32,
        depth: u16,
        entries: u16,
    },
    WouldBlock,
    /// The kernel-facing block backend rejected a mutation because the
    /// underlying device or mount is read-only.
    ReadOnly,
    /// The kernel-facing block backend reported a transport or media I/O
    /// failure. This is distinct from malformed ext4 bytes.
    Io,
    NotEmpty,
    IsDirectory,
    NotDirectory,
    InvalidInput,
}
