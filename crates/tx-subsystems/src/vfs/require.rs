//! VFS require surface — public facade composing walker + witnesses.
//!
//! Per `txdoc:VFS-CHECKS-MODULE-LAYOUT-1`
//! (`docs/design/05_filesystem/VFS_CHECKS_V2.1.md` §3): the require
//! functions are the public entry points that turn a path + context
//! into a typed witness consumed by a VFS step's upgrade sub-phase.
//!
//! Each function:
//! 1. Calls the resolution driver to resolve the path with the
//!    appropriate `WalkMode`.
//! 2. Validates the result shape (directory, entity, parent+name).
//! 3. Constructs an `IdentRef`-carrying witness from the resolved
//!    `PathResolution` via the terminal witness builders.
//!
//! The returned witnesses carry EBR-scoped observations; the consuming
//! step promotes them to `Cap<T>` during STEP-4 phase 2.

use crate::execution::{Errno, Guard};
use crate::vfs::adapter::step_engine::Cap;

use super::checks::{DirectoryAtPath, EntityAtPath, ParentAndName};
use super::resolution::PathResolution;
use super::resolution::driver;
use super::resolution::state::{FinalSymlinkPolicy, WalkMode};
use super::resolution::terminal;
use super::structure::{Credential, DEntry, InlineName};

/// Resolve `path` relative to `rooted_at` and return a terminal
/// entity witness.
///
/// Uses `WalkMode::Entity` — the walker resolves the full path
/// (following symlinks up to `SYMLOOP_MAX` hops) and checks DAC
/// traverse permission at each intermediate directory.  On success
/// the returned `EntityAtPath` carries `IdentRef` handles valid for
/// the duration of `guard`.
pub fn require_entity<'g>(
    rooted_at: Cap<DEntry>,
    path: &[u8],
    cred: &Credential,
    guard: &'g Guard<'_>,
) -> Result<EntityAtPath<'g>, Errno> {
    let resolved = driver::walk_to_completion(
        rooted_at,
        path,
        WalkMode::Entity,
        FinalSymlinkPolicy::Follow,
        cred,
        guard,
    )?;
    terminal::build_entity_witness(&resolved, guard)
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
    let resolved = driver::walk_to_completion(
        rooted_at,
        path,
        WalkMode::Entity,
        FinalSymlinkPolicy::Follow,
        cred,
        guard,
    )?;
    terminal::build_directory_witness(&resolved, guard)
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

    // Find the last separator.
    let (parent_path, name_bytes) = match path.iter().rposition(|&b| b == b'/') {
        None => {
            // No separators — parent is rooted_at.
            let name = if path.is_empty() || path == b"/" {
                return Err(Errno::EINVAL);
            } else {
                path
            };
            let name = InlineName::new(name).map_err(|_| Errno::ENAMETOOLONG)?;
            return Ok(ParentAndName::from_cap(&rooted_at, name, guard));
        }
        Some(pos) => (&path[..pos], &path[pos + 1..]),
    };

    let name = InlineName::new(name_bytes).map_err(|_| Errno::ENAMETOOLONG)?;

    let resolved = if parent_path.is_empty() {
        // path starts with "/" — parent is the namespace root.
        let rnode = rooted_at.rnode().clone();
        let meta = rnode.meta();
        let fs_object_id = rnode.fs_object_id();
        PathResolution {
            dentry: rooted_at,
            rnode,
            fs_object_id,
            meta,
            mount: None,
        }
    } else {
        driver::walk_to_completion(
            rooted_at,
            parent_path,
            WalkMode::ParentAndName,
            FinalSymlinkPolicy::Follow,
            cred,
            guard,
        )?
    };

    terminal::build_parent_and_name_witness(&resolved, name, guard)
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
