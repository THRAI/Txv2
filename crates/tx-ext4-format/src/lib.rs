#![no_std]

extern crate alloc;

pub mod ondisk;
pub mod mapping;
pub mod pager;

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
