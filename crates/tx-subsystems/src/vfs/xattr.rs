//! VFS xattr validation helpers.
//!
//! Filesystem backends own xattr storage. This module only keeps the Linux
//! ABI validation and v1 namespace policy in one place so syscall arms and
//! backends do not drift.

use crate::cred::Capability;
use crate::execution::Errno;

use super::structure::{Credential, InodeMeta};

pub const XATTR_NAME_MAX: usize = 255;
pub const XATTR_SIZE_MAX: usize = 65_536;
pub const XATTR_LIST_MAX: usize = 65_536;
pub const XATTR_CREATE: u32 = 0x1;
pub const XATTR_REPLACE: u32 = 0x2;

pub fn validate_xattr_name(name: &[u8]) -> Result<(), Errno> {
    if name.is_empty() || name.len() > XATTR_NAME_MAX || name.contains(&0) {
        return Err(Errno::ERANGE);
    }
    if !name.starts_with(b"user.") {
        return Err(Errno::EOPNOTSUPP);
    }
    Ok(())
}

pub fn validate_xattr_set_flags(flags: u32) -> Result<(), Errno> {
    if flags & !(XATTR_CREATE | XATTR_REPLACE) != 0 {
        return Err(Errno::EINVAL);
    }
    if flags & XATTR_CREATE != 0 && flags & XATTR_REPLACE != 0 {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

pub fn validate_xattr_value_len(len: usize) -> Result<(), Errno> {
    if len > XATTR_SIZE_MAX {
        Err(Errno::E2BIG)
    } else {
        Ok(())
    }
}

pub fn validate_xattr_list_len(len: usize) -> Result<(), Errno> {
    if len > XATTR_LIST_MAX {
        Err(Errno::E2BIG)
    } else {
        Ok(())
    }
}

pub fn check_xattr_write_perm(meta: &InodeMeta, cred: &Credential) -> Result<(), Errno> {
    if cred.uid == meta.uid || cred.effective_caps.contains(Capability::FOWNER) {
        Ok(())
    } else {
        Err(Errno::EPERM)
    }
}
