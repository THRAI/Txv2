use tx_substrate::epoch::Guard;
use tx_substrate::zone::{Cap, IdentRef};

use crate::mount::structure::{MountIdentity, MountNamespace};
use crate::step::Errno;
use crate::vfs::structure::{DEntry, NameOwned};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WalkMode {
    Entity,
    EntityUnfollowed,
    ParentAndName,
    ParentAndNamedChild,
    EntityOrParentAndName,
    MountPoint,
}

impl WalkMode {
    pub const fn is_parent_mode(self) -> bool {
        matches!(
            self,
            Self::ParentAndName | Self::ParentAndNamedChild | Self::EntityOrParentAndName
        )
    }

    pub(crate) const fn final_symlink_policy(self) -> FinalSymlinkPolicy {
        match self {
            Self::EntityUnfollowed => FinalSymlinkPolicy::Stop,
            _ => FinalSymlinkPolicy::Follow,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FinalSymlinkPolicy {
    Follow,
    Stop,
}

pub struct WalkState<'a, 'g> {
    pub cursor: IdentRef<'g, DEntry>,
    pub remaining: Components<'a>,
    pub symlink_budget: u8,
    pub root_ctx: RootCtxRef<'g>,
    pub trail: WalkTrail<'g>,
    pub current_mount: IdentRef<'g, MountIdentity>,
}

impl<'a, 'g> WalkState<'a, 'g> {
    #[cfg(any(test, feature = "vfs-read-test-support"))]
    pub(crate) fn suspend(&self) -> Result<SuspendedState, Errno> {
        Ok(SuspendedState {
            cursor: self.cursor.to_cap().map_err(|_| Errno::ESTALE)?,
            remaining: OwnedComponents::from_components(self.remaining)?,
            symlink_budget: self.symlink_budget,
            root_ctx: self.root_ctx.to_caps()?,
            trail: self.trail.to_caps()?,
            current_mount: self.current_mount.to_cap().map_err(|_| Errno::ESTALE)?,
        })
    }
}

pub struct RootCtxRef<'g> {
    pub mnt_ns: IdentRef<'g, MountNamespace>,
    pub mnt_ns_root: IdentRef<'g, DEntry>,
    pub chroot: Option<IdentRef<'g, DEntry>>,
    pub cwd: IdentRef<'g, DEntry>,
    pub root_mount: IdentRef<'g, MountIdentity>,
    pub cwd_mount: IdentRef<'g, MountIdentity>,
}

impl<'g> RootCtxRef<'g> {
    #[cfg(any(test, feature = "vfs-read-test-support"))]
    fn to_caps(&self) -> Result<RootCtxCaps, Errno> {
        Ok(RootCtxCaps {
            mnt_ns: self.mnt_ns.to_cap().map_err(|_| Errno::ESTALE)?,
            mnt_ns_root: self.mnt_ns_root.to_cap().map_err(|_| Errno::ESTALE)?,
            chroot: match &self.chroot {
                Some(chroot) => Some(chroot.to_cap().map_err(|_| Errno::ESTALE)?),
                None => None,
            },
            cwd: self.cwd.to_cap().map_err(|_| Errno::ESTALE)?,
            root_mount: self.root_mount.to_cap().map_err(|_| Errno::ESTALE)?,
            cwd_mount: self.cwd_mount.to_cap().map_err(|_| Errno::ESTALE)?,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Components<'a> {
    pub buf: &'a [u8],
    pub cursor: usize,
}

impl<'a> Components<'a> {
    pub const fn new(buf: &'a [u8]) -> Self {
        Self { buf, cursor: 0 }
    }

    pub fn remaining_bytes(&self) -> &'a [u8] {
        if self.cursor >= self.buf.len() {
            &[]
        } else {
            &self.buf[self.cursor..]
        }
    }

    pub fn count(&self) -> usize {
        let mut copy = *self;
        let mut count = 0;
        while let Ok(Some(_)) = copy.next_component() {
            count += 1;
        }
        count
    }

    pub fn is_empty(&self) -> bool {
        self.count() == 0
    }

    pub fn next_component(&mut self) -> Result<Option<&'a [u8]>, Errno> {
        while self.cursor < self.buf.len() && self.buf[self.cursor] == b'/' {
            self.cursor += 1;
        }
        if self.cursor >= self.buf.len() {
            return Ok(None);
        }

        let start = self.cursor;
        while self.cursor < self.buf.len() && self.buf[self.cursor] != b'/' {
            self.cursor += 1;
        }
        let component = &self.buf[start..self.cursor];
        if component.len() > NameOwned::MAX_LEN {
            return Err(Errno::ENAMETOOLONG);
        }
        Ok(Some(component))
    }

    pub fn peek_last_component(&self) -> Result<Option<&'a [u8]>, Errno> {
        let mut copy = *self;
        let mut last = None;
        while let Some(component) = copy.next_component()? {
            last = Some(component);
        }
        Ok(last)
    }
}

pub struct WalkTrail<'g> {
    entries: [Option<TrailEntry<'g>>; 8],
    len: usize,
}

impl<'g> WalkTrail<'g> {
    pub const CAPACITY: usize = 8;

    pub const fn new() -> Self {
        Self {
            entries: [const { None }; 8],
            len: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn push(&mut self, entry: TrailEntry<'g>) -> Result<(), Errno> {
        if self.len == Self::CAPACITY {
            return Err(Errno::ENAMETOOLONG);
        }
        self.entries[self.len] = Some(entry);
        self.len += 1;
        Ok(())
    }

    pub fn pop(&mut self) -> Option<TrailEntry<'g>> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        self.entries[self.len].take()
    }

    #[cfg(any(test, feature = "vfs-read-test-support"))]
    fn to_caps(&self) -> Result<WalkTrailCaps, Errno> {
        let mut caps = WalkTrailCaps::new();
        let mut index = 0;
        while index < self.len {
            match self.entries[index]
                .as_ref()
                .expect("trail entry within len")
            {
                TrailEntry::DEntry(dentry) => {
                    caps.push(TrailEntryCap::DEntry(
                        dentry.to_cap().map_err(|_| Errno::ESTALE)?,
                    ))?;
                }
                TrailEntry::MountBoundary {
                    was_at,
                    was_in_mount,
                } => {
                    caps.push(TrailEntryCap::MountBoundary {
                        was_at: was_at.to_cap().map_err(|_| Errno::ESTALE)?,
                        was_in_mount: was_in_mount.to_cap().map_err(|_| Errno::ESTALE)?,
                    })?;
                }
            }
            index += 1;
        }
        Ok(caps)
    }
}

impl<'g> Default for WalkTrail<'g> {
    fn default() -> Self {
        Self::new()
    }
}

pub enum TrailEntry<'g> {
    DEntry(IdentRef<'g, DEntry>),
    MountBoundary {
        was_at: IdentRef<'g, DEntry>,
        was_in_mount: IdentRef<'g, MountIdentity>,
    },
}

pub enum ResumeToken {
    LookupChild {
        suspended: SuspendedState,
        parent: Cap<DEntry>,
        name: NameOwned,
    },
}

pub struct SuspendedState {
    pub cursor: Cap<DEntry>,
    pub remaining: OwnedComponents,
    pub symlink_budget: u8,
    pub root_ctx: RootCtxCaps,
    pub trail: WalkTrailCaps,
    pub current_mount: Cap<MountIdentity>,
}

impl SuspendedState {
    #[cfg(any(test, feature = "vfs-read-test-support"))]
    pub(crate) fn resume<'s, 'g>(&'s self, guard: &'g Guard<'_>) -> WalkState<'s, 'g> {
        WalkState {
            cursor: self.cursor.ident_ref(guard),
            remaining: self.remaining.components(),
            symlink_budget: self.symlink_budget,
            root_ctx: self.root_ctx.resume(guard),
            trail: self.trail.resume(guard),
            current_mount: self.current_mount.ident_ref(guard),
        }
    }
}

pub struct RootCtxCaps {
    pub mnt_ns: Cap<MountNamespace>,
    pub mnt_ns_root: Cap<DEntry>,
    pub chroot: Option<Cap<DEntry>>,
    pub cwd: Cap<DEntry>,
    pub root_mount: Cap<MountIdentity>,
    pub cwd_mount: Cap<MountIdentity>,
}

impl RootCtxCaps {
    fn resume<'g>(&self, guard: &'g Guard<'_>) -> RootCtxRef<'g> {
        RootCtxRef {
            mnt_ns: self.mnt_ns.ident_ref(guard),
            mnt_ns_root: self.mnt_ns_root.ident_ref(guard),
            chroot: self.chroot.as_ref().map(|cap| cap.ident_ref(guard)),
            cwd: self.cwd.ident_ref(guard),
            root_mount: self.root_mount.ident_ref(guard),
            cwd_mount: self.cwd_mount.ident_ref(guard),
        }
    }
}

// RFX-VFS-P3-001: Allow caller-configurable symlink budget via ResolveCtx once
// process/thread symlink depth tracking is available.
pub(crate) const DEFAULT_SYMLINK_BUDGET: u8 = 40;

/// Construct the initial [`WalkState`] for a new path walk.
///
/// Absolute paths (leading `/`) start from `mnt_ns_root` + `root_mount`.
/// Relative paths start from `cwd` + `cwd_mount`.
///
/// # RFX-VFS-P3-007
/// When `chroot` is `Some`, absolute paths should start from the chroot root
/// instead of `mnt_ns_root`. Deferred until PROCESS layer supplies a
/// `chroot_mount` alongside the chroot DEntry.
///
/// # RFX-VFS-P3-002
/// `AT_FDCWD` / `AT_EMPTY_PATH` dirfd semantics are not handled here.
/// Deferred until `openat`/`faccessat` syscall variants pass dirfd context
/// through `ResolveCtx`.
pub(crate) fn make_initial_walk_state<'a, 'g>(
    path: &'a [u8],
    root_ctx_caps: &RootCtxCaps,
    guard: &'g Guard<'_>,
) -> Result<WalkState<'a, 'g>, Errno> {
    if path.is_empty() {
        return Err(Errno::ENOENT);
    }
    let root_ctx = root_ctx_caps.resume(guard);
    let (cursor, current_mount) = if path.first() == Some(&b'/') {
        (
            root_ctx_caps.mnt_ns_root.ident_ref(guard),
            root_ctx_caps.root_mount.ident_ref(guard),
        )
    } else {
        (
            root_ctx_caps.cwd.ident_ref(guard),
            root_ctx_caps.cwd_mount.ident_ref(guard),
        )
    };
    Ok(WalkState {
        cursor,
        remaining: Components::new(path),
        symlink_budget: DEFAULT_SYMLINK_BUDGET,
        root_ctx,
        trail: WalkTrail::new(),
        current_mount,
    })
}

pub struct OwnedComponents {
    pub len: u16,
    pub bytes: [u8; 4096],
    pub cursor: usize,
}

impl OwnedComponents {
    pub const MAX_LEN: usize = 4096;

    pub fn from_components(components: Components<'_>) -> Result<Self, Errno> {
        if components.buf.len() > Self::MAX_LEN {
            return Err(Errno::ENAMETOOLONG);
        }
        let mut bytes = [0; Self::MAX_LEN];
        bytes[..components.buf.len()].copy_from_slice(components.buf);
        Ok(Self {
            len: components.buf.len() as u16,
            bytes,
            cursor: components.cursor,
        })
    }

    pub fn components(&self) -> Components<'_> {
        Components {
            buf: &self.bytes[..self.len as usize],
            cursor: self.cursor,
        }
    }
}

pub struct WalkTrailCaps {
    entries: [Option<TrailEntryCap>; 8],
    len: usize,
}

impl WalkTrailCaps {
    pub const CAPACITY: usize = 8;

    pub const fn new() -> Self {
        Self {
            entries: [const { None }; 8],
            len: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn push(&mut self, entry: TrailEntryCap) -> Result<(), Errno> {
        if self.len == Self::CAPACITY {
            return Err(Errno::ENAMETOOLONG);
        }
        self.entries[self.len] = Some(entry);
        self.len += 1;
        Ok(())
    }

    #[cfg(any(test, feature = "vfs-read-test-support"))]
    fn resume<'g>(&self, guard: &'g Guard<'_>) -> WalkTrail<'g> {
        let mut trail = WalkTrail::new();
        let mut index = 0;
        while index < self.len {
            let entry = match self.entries[index]
                .as_ref()
                .expect("trail cap entry within len")
            {
                TrailEntryCap::DEntry(dentry) => TrailEntry::DEntry(dentry.ident_ref(guard)),
                TrailEntryCap::MountBoundary {
                    was_at,
                    was_in_mount,
                } => TrailEntry::MountBoundary {
                    was_at: was_at.ident_ref(guard),
                    was_in_mount: was_in_mount.ident_ref(guard),
                },
            };
            trail
                .push(entry)
                .expect("resuming equal-size trail cannot overflow");
            index += 1;
        }
        trail
    }
}

impl Default for WalkTrailCaps {
    fn default() -> Self {
        Self::new()
    }
}

pub enum TrailEntryCap {
    DEntry(Cap<DEntry>),
    MountBoundary {
        was_at: Cap<DEntry>,
        was_in_mount: Cap<MountIdentity>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walk_mode_classifies_parent_family() {
        assert!(!WalkMode::Entity.is_parent_mode());
        assert!(!WalkMode::EntityUnfollowed.is_parent_mode());
        assert!(WalkMode::ParentAndName.is_parent_mode());
        assert!(WalkMode::ParentAndNamedChild.is_parent_mode());
        assert!(WalkMode::EntityOrParentAndName.is_parent_mode());
        assert!(!WalkMode::MountPoint.is_parent_mode());
    }

    #[test]
    fn walk_mode_maps_final_symlink_policy() {
        assert_eq!(
            WalkMode::EntityUnfollowed.final_symlink_policy(),
            FinalSymlinkPolicy::Stop
        );
        assert_eq!(
            WalkMode::Entity.final_symlink_policy(),
            FinalSymlinkPolicy::Follow
        );
    }

    #[test]
    fn components_skip_repeated_and_trailing_slashes() {
        let mut components = Components::new(b"/a//b/");

        assert_eq!(components.next_component(), Ok(Some(&b"a"[..])));
        assert_eq!(components.next_component(), Ok(Some(&b"b"[..])));
        assert_eq!(components.next_component(), Ok(None));
    }

    #[test]
    fn components_preserve_dot_components() {
        let mut dot = Components::new(b".");
        let mut dotdot = Components::new(b"..");

        assert_eq!(dot.next_component(), Ok(Some(&b"."[..])));
        assert_eq!(dotdot.next_component(), Ok(Some(&b".."[..])));
    }

    #[test]
    fn components_count_ignores_empty_segments() {
        assert_eq!(Components::new(b"/a//b/").count(), 2);
        assert!(Components::new(b"///").is_empty());
    }

    #[test]
    fn components_reject_overlong_name() {
        let bytes = [b'a'; NameOwned::MAX_LEN + 1];
        let mut components = Components::new(&bytes);

        assert_eq!(components.next_component(), Err(Errno::ENAMETOOLONG));
    }

    #[test]
    fn resume_token_carries_owned_caps_not_guard_borrows() {
        fn assert_static<T: 'static>() {}

        assert_static::<ResumeToken>();
        assert_static::<SuspendedState>();
        assert!(core::mem::size_of::<ResumeToken>() > 0);
    }
}
