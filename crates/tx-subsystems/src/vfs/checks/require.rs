use tx_substrate::epoch::Guard;

use crate::step::Errno;
use crate::vfs::checks::predicates;
use crate::vfs::checks::resolution::driver::{self, DriverStep};
use crate::vfs::checks::resolution::state::{make_initial_walk_state, RootCtxCaps, WalkMode};
use crate::vfs::checks::witness::{
    DirectoryAtPath, EntityAtPath, EntityOrParentAndName, MountPointAtPath, ParentAndName,
    ParentAndNamedChild, RealPath, RmdirableDirChild, SymlinkAtPath, UnlinkableNonDirChild,
    WalkWitness,
};

pub type VfsError = Errno;

pub struct ResolveCtx {
    caps: RootCtxCaps,
}

impl ResolveCtx {
    pub fn new(caps: RootCtxCaps) -> Self {
        // RFX-VFS-P1-012 (continued): Replace explicit RootCtxCaps construction
        // with ResolveCtx::from_process(process_ref) once PROCESS frame/fs
        // context API is available.
        Self { caps }
    }

    pub(crate) fn root_ctx_caps(&self) -> &RootCtxCaps {
        &self.caps
    }
}

/// Internal helper: build initial state, run walker, map NeedIO to NotImplemented.
fn run_require<'a, 'g>(
    path: &'a [u8],
    ctx: &ResolveCtx,
    guard: &'g Guard<'_>,
    mode: WalkMode,
) -> Result<WalkWitness<'g>, VfsError> {
    let state = make_initial_walk_state(path, &ctx.caps, guard)?;
    match driver::run_walker(mode, state, guard) {
        DriverStep::Accept(witness) => Ok(witness),
        DriverStep::Error(e) => Err(e),
        // RFX-VFS-P3-003: Replace with async IO dispatch once cold IO path
        // and dcache population are implemented.
        DriverStep::NeedIO(_, _) => Err(Errno::NotImplemented),
    }
}

// ── Core facades ─────────────────────────────────────────────────────────────

pub fn require_entity<'g>(
    path: &[u8],
    ctx: &ResolveCtx,
    guard: &'g Guard<'_>,
) -> Result<EntityAtPath<'g>, VfsError> {
    match run_require(path, ctx, guard, WalkMode::Entity)? {
        WalkWitness::Entity(e) => Ok(e),
        // Mode/witness invariant: driver only produces Entity witness for
        // Entity mode. Reaching here would be a driver bug.
        _ => Err(Errno::Invalid),
    }
}

pub fn require_entity_unfollowed<'g>(
    path: &[u8],
    ctx: &ResolveCtx,
    guard: &'g Guard<'_>,
) -> Result<EntityAtPath<'g>, VfsError> {
    match run_require(path, ctx, guard, WalkMode::EntityUnfollowed)? {
        WalkWitness::EntityUnfollowed(e) => Ok(e),
        _ => Err(Errno::Invalid),
    }
}

pub fn require_parent_and_name<'g>(
    path: &[u8],
    ctx: &ResolveCtx,
    guard: &'g Guard<'_>,
) -> Result<ParentAndName<'g>, VfsError> {
    match run_require(path, ctx, guard, WalkMode::ParentAndName)? {
        WalkWitness::ParentAndName(p) => Ok(p),
        _ => Err(Errno::Invalid),
    }
}

pub fn require_parent_and_named_child<'g>(
    path: &[u8],
    ctx: &ResolveCtx,
    guard: &'g Guard<'_>,
) -> Result<ParentAndNamedChild<'g>, VfsError> {
    match run_require(path, ctx, guard, WalkMode::ParentAndNamedChild)? {
        WalkWitness::ParentAndNamedChild(p) => Ok(p),
        _ => Err(Errno::Invalid),
    }
}

pub fn require_entity_or_parent_and_name<'g>(
    path: &[u8],
    ctx: &ResolveCtx,
    guard: &'g Guard<'_>,
) -> Result<EntityOrParentAndName<'g>, VfsError> {
    match run_require(path, ctx, guard, WalkMode::EntityOrParentAndName)? {
        WalkWitness::EntityOrParent(e) => Ok(e),
        _ => Err(Errno::Invalid),
    }
}

pub fn require_mount_point<'g>(
    _path: &[u8],
    _ctx: &ResolveCtx,
    _guard: &'g Guard<'_>,
) -> Result<MountPointAtPath<'g>, VfsError> {
    // RFX-VFS-P3-004: Wire to run_require(WalkMode::MountPoint) once
    // mount::checks::is_mount_root returns real topology truth. Current MOUNT
    // checks stub always returns false so the walker never accepts and
    // terminates with Errno::NoEntry — kept as NotImplemented to avoid
    // misleading callers.
    Err(Errno::NotImplemented)
}

pub fn require_real_path<'g>(
    _path: &[u8],
    _ctx: &ResolveCtx,
    _guard: &'g Guard<'_>,
) -> Result<RealPath<'g>, VfsError> {
    // RFX-VFS-P3-005: Implement path reconstruction once DEntry carries a
    // parent pointer or trail-based reverse walk is available.
    Err(Errno::NotImplemented)
}

// ── Refinement wrappers ───────────────────────────────────────────────────────

pub fn require_directory<'g>(
    path: &[u8],
    ctx: &ResolveCtx,
    guard: &'g Guard<'_>,
) -> Result<DirectoryAtPath<'g>, VfsError> {
    let entity = require_entity(path, ctx, guard)?;
    if !predicates::is_directory(&entity.rnode) {
        return Err(Errno::NotDirectory);
    }
    Ok(DirectoryAtPath::new(entity))
}

pub fn require_symlink_for_readlink<'g>(
    path: &[u8],
    ctx: &ResolveCtx,
    guard: &'g Guard<'_>,
) -> Result<SymlinkAtPath<'g>, VfsError> {
    let entity = require_entity_unfollowed(path, ctx, guard)?;
    if !predicates::is_symlink(&entity.rnode) {
        return Err(Errno::Invalid);
    }
    Ok(SymlinkAtPath::new(entity))
}

pub fn require_unlinkable_non_dir_child<'g>(
    path: &[u8],
    ctx: &ResolveCtx,
    guard: &'g Guard<'_>,
) -> Result<UnlinkableNonDirChild<'g>, VfsError> {
    let child = require_parent_and_named_child(path, ctx, guard)?;
    if predicates::is_directory(child.child_rnode()) {
        return Err(Errno::IsDirectory);
    }
    Ok(UnlinkableNonDirChild::new(child))
}

pub fn require_rmdirable_dir_child<'g>(
    path: &[u8],
    ctx: &ResolveCtx,
    guard: &'g Guard<'_>,
) -> Result<RmdirableDirChild<'g>, VfsError> {
    let child = require_parent_and_named_child(path, ctx, guard)?;
    if !predicates::is_directory(child.child_rnode()) {
        return Err(Errno::NotDirectory);
    }
    // RFX-VFS-P1-004: Directory emptiness requires backend/readdir IO.
    Err(Errno::NotImplemented)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tx_substrate::{epoch, zone};

    use crate::mount::structure::testing::{
        make_bootstrap_pair_for_test, make_dentry_for_test, make_rnode_for_test,
    };
    use crate::vfs::checks::resolution::state::RootCtxCaps;
    use crate::vfs::structure::{InodeMeta, NameOwned};
    use tx_substrate::zone::Cap;

    fn setup() {
        tx_substrate::testing::init_host_for_test_once();
        let _ = zone::register_zone_for::<crate::vfs::structure::RNode>();
        let _ = zone::register_zone_for::<crate::vfs::structure::DEntry>();
        let _ = zone::register_zone_for::<crate::mount::structure::MountIdentity>();
        let _ = zone::register_zone_for::<crate::mount::structure::MountNamespace>();
    }

    fn make_root_and_child() -> (
        Cap<crate::vfs::structure::DEntry>,
        Cap<crate::vfs::structure::DEntry>,
    ) {
        let root_rn = make_rnode_for_test(1, InodeMeta::TYPE_DIRECTORY | 0o755);
        let bar_rn = make_rnode_for_test(2, InodeMeta::TYPE_DIRECTORY | 0o755);
        let root_cap = make_dentry_for_test(1, b".", root_rn);
        let bar_cap = make_dentry_for_test(2, b"bar", bar_rn);
        (root_cap, bar_cap)
    }

    fn keep_fixture_live(
        root_cap: Cap<crate::vfs::structure::DEntry>,
        mi_cap: Cap<crate::mount::structure::MountIdentity>,
        ns_cap: Cap<crate::mount::structure::MountNamespace>,
    ) {
        core::mem::forget(root_cap);
        core::mem::forget(mi_cap);
        core::mem::forget(ns_cap);
    }

    // G.2.1: absolute path /bar finds existing child via warm-path walker.
    #[test]
    fn require_entity_absolute_path_finds_child() {
        let _serial = crate::test_support::EpochTestGuard::acquire();
        setup();
        let (root_cap, bar_cap) = make_root_and_child();

        let guard = epoch::guard();
        root_cap
            .ident_ref(&guard)
            .children
            .install_committed_for_test_or_bootstrap(
                NameOwned::from_component(b"bar").unwrap(),
                bar_cap,
            )
            .expect("install bar");
        drop(guard);

        let (mi_cap, ns_cap) = make_bootstrap_pair_for_test(root_cap.clone());
        let guard = epoch::guard();
        let ctx = ResolveCtx::new(RootCtxCaps {
            mnt_ns: ns_cap.clone(),
            mnt_ns_root: root_cap.clone(),
            chroot: None,
            cwd: root_cap.clone(),
            root_mount: mi_cap.clone(),
            cwd_mount: mi_cap.clone(),
        });

        let result = require_entity(b"/bar", &ctx, &guard);
        assert!(result.is_ok(), "expected Ok, got {:?}", result.err());
        drop(result);
        drop(guard);
        keep_fixture_live(root_cap, mi_cap, ns_cap);
    }

    // G.2.2: require_entity_unfollowed returns the entry without following symlinks.
    #[test]
    fn require_entity_unfollowed_absolute_path_finds_child() {
        let _serial = crate::test_support::EpochTestGuard::acquire();
        setup();
        let (root_cap, bar_cap) = make_root_and_child();

        let guard = epoch::guard();
        root_cap
            .ident_ref(&guard)
            .children
            .install_committed_for_test_or_bootstrap(
                NameOwned::from_component(b"bar").unwrap(),
                bar_cap,
            )
            .expect("install bar");
        drop(guard);

        let (mi_cap, ns_cap) = make_bootstrap_pair_for_test(root_cap.clone());
        let guard = epoch::guard();
        let ctx = ResolveCtx::new(RootCtxCaps {
            mnt_ns: ns_cap.clone(),
            mnt_ns_root: root_cap.clone(),
            chroot: None,
            cwd: root_cap.clone(),
            root_mount: mi_cap.clone(),
            cwd_mount: mi_cap.clone(),
        });

        let result = require_entity_unfollowed(b"/bar", &ctx, &guard);
        assert!(result.is_ok(), "expected Ok, got {:?}", result.err());
        drop(result);
        drop(guard);
        keep_fixture_live(root_cap, mi_cap, ns_cap);
    }

    // G.2.3: parent-or-create probe returns Absent for a known missing final child.
    #[test]
    fn require_entity_or_parent_and_name_missing_path_returns_absent() {
        let _serial = crate::test_support::EpochTestGuard::acquire();
        setup();
        let root_rn = make_rnode_for_test(10, InodeMeta::TYPE_DIRECTORY | 0o755);
        let root_cap = make_dentry_for_test(10, b".", root_rn);

        let (mi_cap, ns_cap) = make_bootstrap_pair_for_test(root_cap.clone());
        let guard = epoch::guard();
        let ctx = ResolveCtx::new(RootCtxCaps {
            mnt_ns: ns_cap.clone(),
            mnt_ns_root: root_cap.clone(),
            chroot: None,
            cwd: root_cap.clone(),
            root_mount: mi_cap.clone(),
            cwd_mount: mi_cap.clone(),
        });

        let result = require_entity_or_parent_and_name(b"/missing", &ctx, &guard)
            .expect("known empty root children should produce Absent witness");
        assert!(result.is_absent());
        drop(result);
        drop(guard);
        keep_fixture_live(root_cap, mi_cap, ns_cap);
    }
}
