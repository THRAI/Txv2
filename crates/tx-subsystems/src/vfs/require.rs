//! VFS require surface — public facade composing walker + witnesses.
//!
//! Per `txdoc:VFS-CHECKS-MODULE-LAYOUT-1`
//! (`docs/design/05_filesystem/VFS_CHECKS_V2.1.md` §3): the require
//! functions are the public entry points that turn a path + context
//! into a typed witness consumed by a VFS step's upgrade sub-phase.
//!
//! Each function:
//! 1. Calls the walker to resolve the path into a terminal `Cap<DEntry>`.
//! 2. Validates the result shape (directory, entity, parent+name).
//! 3. Constructs an `IdentRef`-carrying witness from the resolved caps
//!    under the caller's epoch guard.
//!
//! The returned witnesses carry EBR-scoped observations; the consuming
//! step promotes them to `Cap<T>` during STEP-4 phase 2.

use crate::execution::{Errno, Guard};
use crate::vfs::adapter::step_engine::{Cap, NoProgress, StepOutcome};

use super::checks::{DirectoryAtPath, EntityAtPath, ParentAndName};
use super::structure::{Credential, DEntry, InlineName, InodeKind, RNode, VfsName};
use super::walker::{step_walk, SYMLOOP_MAX};

/// Resolve `path` relative to `rooted_at` and return a terminal
/// entity witness.
///
/// The walker resolves the full path (following symlinks up to
/// `SYMLOOP_MAX` hops) and checks DAC traverse permission at each
/// intermediate directory.  On success the returned `EntityAtPath`
/// carries `IdentRef` handles valid for the duration of `guard`.
pub fn require_entity<'g>(
    rooted_at: Cap<DEntry>,
    path: &[u8],
    cred: &Credential,
    guard: &'g Guard<'_>,
) -> Result<EntityAtPath<'g>, Errno> {
    let dentry = match step_walk(rooted_at, path, cred, guard) {
        StepOutcome::Done(d) => d,
        StepOutcome::Err(e) => return Err(Errno::from(e)),
        _ => return Err(Errno::EIO),
    };
    let rnode = dentry.rnode();
    Ok(EntityAtPath::from_caps(&dentry, &rnode, guard))
}

/// Resolve `path` to a directory witness.
///
/// Same resolution as [`require_entity`], but additionally validates
/// that the terminal RNode represents a directory.  Returns `ENOTDIR`
/// if the resolved entity is not a directory.
pub fn require_directory<'g>(
    rooted_at: Cap<DEntry>,
    path: &[u8],
    cred: &Credential,
    guard: &'g Guard<'_>,
) -> Result<DirectoryAtPath<'g>, Errno> {
    let dentry = match step_walk(rooted_at, path, cred, guard) {
        StepOutcome::Done(d) => d,
        StepOutcome::Err(e) => return Err(Errno::from(e)),
        _ => return Err(Errno::EIO),
    };
    let rnode = dentry.rnode();
    if rnode.meta().kind() != InodeKind::Directory {
        return Err(Errno::ENOTDIR);
    }
    Ok(DirectoryAtPath::from_caps(&dentry, &rnode, guard))
}

/// Walk to the parent of the final component and return a
/// `ParentAndName` witness.
///
/// `path` is split at the last `/` separator.  The prefix is walked
/// to obtain the parent `Cap<DEntry>`; the suffix is validated as a
/// `VfsName`.  Returns the parent's `IdentRef` plus the validated
/// `InlineName`.
///
/// If `path` has no separator (single-component path), the parent is
/// `rooted_at` and the name is the entire path.
pub fn require_parent_and_name<'g>(
    rooted_at: Cap<DEntry>,
    path: &[u8],
    cred: &Credential,
    guard: &'g Guard<'_>,
) -> Result<ParentAndName<'g>, Errno> {
    // Strip trailing slashes.
    let path = strip_trailing_slashes(path);

    // Split into (parent_path, final_component).
    let (parent_path, name_bytes) = match path.rsplitn(2, |&b| b == b'/').next() {
        Some(b"") | None => {
            // No separators, or the path IS "/" — parent is rooted_at.
            let name = if path.is_empty() || path == b"/" {
                return Err(Errno::EINVAL);
            } else {
                path
            };
            let name = InlineName::new(name).map_err(|_| Errno::ENAMETOOLONG)?;
            return Ok(ParentAndName::from_cap(&rooted_at, name, guard));
        }
        Some(name_bytes) => {
            // Find the split point. rsplitn gives us the last segment.
            // We need to manually split.
            let last_slash = path.iter().rposition(|&b| b == b'/').unwrap();
            let parent_bytes = &path[..last_slash];
            let name_bytes = &path[last_slash + 1..];
            (parent_bytes, name_bytes)
        }
    };

    let name = InlineName::new(name_bytes).map_err(|_| Errno::ENAMETOOLONG)?;

    let parent_dentry = if parent_path.is_empty() {
        rooted_at
    } else {
        match step_walk(rooted_at, parent_path, cred, guard) {
            StepOutcome::Done(d) => d,
            StepOutcome::Err(e) => return Err(Errno::from(e)),
            _ => return Err(Errno::EIO),
        }
    };

    Ok(ParentAndName::from_cap(&parent_dentry, name, guard))
}

/// Strip trailing `/` bytes from a path slice.
fn strip_trailing_slashes(mut path: &[u8]) -> &[u8] {
    while path.len() > 1 && path.last() == Some(&b'/') {
        path = &path[..path.len() - 1];
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_trailing_slashes_removes_single() {
        assert_eq!(strip_trailing_slashes(b"/tmp/"), b"/tmp");
    }

    #[test]
    fn strip_trailing_slashes_removes_multiple() {
        assert_eq!(strip_trailing_slashes(b"/tmp///"), b"/tmp");
    }

    #[test]
    fn strip_trailing_slashes_preserves_root() {
        assert_eq!(strip_trailing_slashes(b"/"), b"/");
    }

    #[test]
    fn strip_trailing_slashes_empty_is_unchanged() {
        assert_eq!(strip_trailing_slashes(b""), b"");
    }
}
