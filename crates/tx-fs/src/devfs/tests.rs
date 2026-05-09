//! Phase 3a tests: devfs as a standalone backend.
//!
//! Mount wiring (Phase 3b) is deliberately out of scope. These tests
//! construct a fake hardware TTY in-process via the existing
//! `tty::execution::register_hardware` + `register_console_alias`
//! seam, then exercise the devfs `FsOps` surface and
//! `open_console_for_init` directly.

use alloc::boxed::Box;
use alloc::vec::Vec;

use std::sync::Mutex;

use tx_subsystems::device::{CharDeviceBinding, CharDeviceOps, DevT};
use tx_subsystems::execution::{Guard, StepOutcome};
use tx_subsystems::tty::execution::{register_console_alias, register_hardware};
use tx_subsystems::vfs::{
    Credential, DirCursor, FsObjectId, FsOps, RNodeBacking, StructPayload,
};
use tx_substrate::step_v3::{Errno as V3Errno, StepOutcome as V3Outcome};

use super::{open_console_for_init, Devfs, DEVFS_ROOT_OBJECT_ID};

fn init_tty_zones() {
    // Idempotent: `tx_subsystems::zones::register_all()` calls
    // `register_zone_for::<T>()` per subsystem, and the underlying
    // `register_static_zone` is idempotent for the same static zone
    // (see `crates/tx-substrate/src/zone/registry.rs`). We do not call
    // `reset_for_tests` because that helper is `#[cfg(test)]` inside
    // tx-subsystems and not reachable from this crate; instead, we
    // serialize tests with `DEVFS_TEST_LOCK` and rely on
    // `register_hardware` / `register_console_alias` overwriting
    // same-name entries (the alias table is a fixed-slot in-place
    // upsert per `crates/tx-subsystems/src/tty/structure/registry.rs`).
    tx_substrate::testing::init_host_for_test_once();
    tx_subsystems::zones::register_all().expect("tx-subsystems zones");
}

// --- fake hardware binding ------------------------------------------------

/// Capturing char-device binding so tests can assert on the bytes that
/// reach `tty::execution::step_write`'s underlying transport.
struct CapturingOps {
    captured: Mutex<Vec<u8>>,
}

impl CapturingOps {
    fn new() -> Self {
        Self {
            captured: Mutex::new(Vec::new()),
        }
    }

    fn snapshot(&self) -> Vec<u8> {
        self.captured.lock().expect("capture lock").clone()
    }
}

impl CharDeviceOps for CapturingOps {
    fn read(
        &self,
        _out: &mut [u8],
        _guard: &Guard<'_>,
    ) -> V3Outcome<usize, tx_substrate::step_v3::ByteProgress> {
        V3Outcome::Done(0)
    }

    fn write(
        &self,
        bytes: &[u8],
        _guard: &Guard<'_>,
    ) -> V3Outcome<usize, tx_substrate::step_v3::ByteProgress> {
        self.captured
            .lock()
            .expect("capture lock")
            .extend_from_slice(bytes);
        V3Outcome::Done(bytes.len())
    }
}

/// Register a capturing fake hardware TTY at `ttyS0` and alias it as
/// `/dev/console`. Returns a `'static` reference to the capturing ops
/// so the caller can read the captured-bytes buffer after invoking
/// step_write through the OpenFile path.
fn install_capturing_console() -> &'static CapturingOps {
    let ops_static: &'static CapturingOps = Box::leak(Box::new(CapturingOps::new()));
    let binding = Box::leak(Box::new(CharDeviceBinding {
        devt: DevT::new(4, 64),
        name: "console-test",
        ops: ops_static,
    }));
    let guard = tx_substrate::epoch::guard();
    let tty = match register_hardware("console-hw", 0, binding, &guard) {
        StepOutcome::Done(tty) => tty,
        other => panic!("register_hardware failed: {other:?}"),
    };
    assert_eq!(
        register_console_alias("console", tty),
        tx_substrate::step_v3::StepOutcome::Done(())
    );
    ops_static
}

// --- tests ----------------------------------------------------------------

#[test]
fn devfs_lookup_console_after_register_hardware_returns_tty_rnode() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_tty_zones();

    let _ops = install_capturing_console();

    let guard = tx_substrate::epoch::guard();
    let devfs = Devfs::new();

    // FsOps::lookup over the devfs root yields a stable FsObjectId
    // for the alias.
    let obj_id = match <Devfs as FsOps>::lookup(&devfs, DEVFS_ROOT_OBJECT_ID, b"console", &guard)
    {
        V3Outcome::Done(id) => id,
        other => panic!("devfs.lookup(console) failed: {other:?}"),
    };

    // Materialise an RNode through the project layer (this is the same
    // shape the future VFS walker will produce on `step_open`).
    let rnode = match super::resolve_console_rnode(b"console") {
        StepOutcome::Done(rnode) => rnode,
        other => panic!("resolve_console_rnode(console) failed: {other:?}"),
    };

    // The RNode must point at the registered TTY via StructBacked { Tty }.
    let registered = tx_subsystems::tty::project::resolve_devfs_alias(b"console")
        .expect("console alias should be registered");
    match rnode.backing() {
        RNodeBacking::StructBacked {
            payload: StructPayload::Tty(rnode_tty),
        } => assert_eq!(*rnode_tty, registered),
        other => panic!("expected StructBacked::Tty, got {other:?}"),
    }

    // Lookup result is consistent with the entry's index in the alias
    // snapshot (sanity: the id is non-root, non-zero).
    assert_ne!(obj_id, DEVFS_ROOT_OBJECT_ID);
}

#[test]
fn open_console_for_init_now_routes_through_walker_with_legacy_fallback() {
    // Phase 4 retires the bootstrap exemption: when the walker can
    // resolve `/dev/console` (init_process bound, mount table
    // populated, dentry tree wired), `open_console_for_init` returns
    // through `vfs::step_open`. When those preconditions aren't yet
    // met (e.g. these tx-fs tests don't bootstrap init), the helper
    // falls back to the legacy direct-RNode path.
    //
    // The fallback path is exercised by every existing devfs test
    // here that calls `open_console_for_init` (no INIT_PROCESS in
    // scope → walker not consulted → legacy direct-RNode path runs).
    // This test asserts the OpenFile shape end-to-end through the
    // legacy fallback so a regression in that branch shows up.
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_tty_zones();

    let _ops = install_capturing_console();
    let console = open_console_for_init();
    let registered = tx_subsystems::tty::project::resolve_devfs_alias(b"console")
        .expect("console alias should be registered");
    match console.rnode().backing() {
        RNodeBacking::StructBacked {
            payload: StructPayload::Tty(tty),
        } => assert_eq!(*tty, registered),
        other => panic!("expected StructBacked::Tty, got {other:?}"),
    }
}

#[test]
fn devfs_write_through_openfile_reaches_tty_step_write() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_tty_zones();

    let ops = install_capturing_console();

    let guard = tx_substrate::epoch::guard();
    let console = open_console_for_init();

    // step_write goes: OpenFile::step_write -> RNodeBacking::StructBacked
    // { Tty } -> tty::execution::step_write -> CharDeviceBinding::write
    // (our capturing ops). N_TTY's default-cooked output discipline
    // applies OPOST|ONLCR (LF -> CR LF) before bytes hit the transport,
    // which is what we want to verify lands on the device — proving the
    // path goes through TTY's ldisc rather than dropping straight onto
    // the binding.
    match console.step_write(b"hi\n", &guard) {
        V3Outcome::Done(written) => assert_eq!(written, 3),
        other => panic!("step_write failed: {other:?}"),
    }

    let captured = ops.snapshot();
    assert_eq!(
        captured, b"hi\r\n",
        "step_write should transit ldisc post-processing (ONLCR) before reaching the binding"
    );
}

#[test]
fn devfs_create_returns_erofs() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_tty_zones();

    let guard = tx_substrate::epoch::guard();
    let devfs = Devfs::new();

    // Devfs always returns EROFS for mutators, so cred privilege does
    // not affect the assertion; leaving as `Credential::default()`
    // documents that this test is not gated on DAC behaviour.
    let cred = Credential::default();
    let outcome = <Devfs as FsOps>::create_inode(
        &devfs,
        DEVFS_ROOT_OBJECT_ID,
        b"new-thing",
        0o100644,
        &cred,
        &guard,
    );
    assert_eq!(outcome, V3Outcome::err(V3Errno::EROFS));

    // Other mutators report the same. Spot-check the most likely
    // accidental-success paths:
    assert_eq!(
        <Devfs as FsOps>::mkdir(&devfs, DEVFS_ROOT_OBJECT_ID, b"sub", 0o755, &cred, &guard),
        V3Outcome::err(V3Errno::EROFS)
    );
    assert_eq!(
        <Devfs as FsOps>::unlink(
            &devfs,
            DEVFS_ROOT_OBJECT_ID,
            b"console",
            FsObjectId::new(0),
            &guard
        ),
        V3Outcome::err(V3Errno::EROFS)
    );
    assert_eq!(
        <Devfs as FsOps>::serialize_inode_meta(
            &devfs,
            DEVFS_ROOT_OBJECT_ID,
            &tx_subsystems::vfs::InodeMeta::new(tx_subsystems::vfs::InodeKind::Directory, 0o755),
            &guard,
        ),
        V3Outcome::err(V3Errno::EROFS)
    );
}

#[test]
fn devfs_chmod_returns_erofs() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_tty_zones();

    let guard = tx_substrate::epoch::guard();
    let devfs = Devfs::new();
    let cred = Credential::root();

    // devfs nodes are kernel-owned; mode-bit mutation is not
    // supported. The override returns EROFS regardless of
    // privilege.
    assert_eq!(
        FsOps::step_chmod(&devfs, DEVFS_ROOT_OBJECT_ID, 0o700, &cred, &guard),
        V3Outcome::err(V3Errno::EROFS)
    );
}

#[test]
fn devfs_chown_returns_erofs() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_tty_zones();

    let guard = tx_substrate::epoch::guard();
    let devfs = Devfs::new();
    let cred = Credential::root();

    assert_eq!(
        FsOps::step_chown(
            &devfs,
            DEVFS_ROOT_OBJECT_ID,
            Some(1000),
            Some(1000),
            &cred,
            &guard,
        ),
        V3Outcome::err(V3Errno::EROFS)
    );
}

#[test]
fn devfs_lookup_unknown_returns_enoent() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_tty_zones();

    // Register `console` so the registry is populated but
    // `not-a-real-device` still misses.
    let _ops = install_capturing_console();

    let guard = tx_substrate::epoch::guard();
    let devfs = Devfs::new();

    assert_eq!(
        <Devfs as FsOps>::lookup(&devfs, DEVFS_ROOT_OBJECT_ID, b"not-a-real-device", &guard),
        V3Outcome::err(V3Errno::ENOENT)
    );

    // Non-root parent always misses too.
    assert_eq!(
        <Devfs as FsOps>::lookup(&devfs, FsObjectId::new(0xdead_beef), b"console", &guard),
        V3Outcome::err(V3Errno::ENOENT)
    );
}

// === FsOps / FsPageBacking trait shape tests =========================
//
// Pin the outcome shape on `Devfs` so a regression surfaces locally
// rather than at the walker call site. Tests exercise the most
// representative methods: `lookup` (positive + negative),
// `load_inode_meta` (root directory), and `fetch_page` (devfs's
// distinctive `ENOSYS` rejection — char-device I/O does not flow
// through the page cache).

#[test]
fn devfs_v3_lookup_console_returns_done_with_object_id() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_tty_zones();

    let _ops = install_capturing_console();

    use tx_subsystems::vfs::FsOps;
    use tx_substrate::step_v3::StepOutcome as V3;

    let devfs = Devfs::new();
    let guard = tx_substrate::epoch::guard();

    // Positive lookup: console alias is registered → `Done(id)` with a
    // non-root id.
    let id = match <Devfs as FsOps>::lookup(&devfs, DEVFS_ROOT_OBJECT_ID, b"console", &guard) {
        V3::Done(id) => id,
        other => panic!("v3 lookup(console): {other:?}"),
    };
    assert_ne!(id, DEVFS_ROOT_OBJECT_ID);

    // Negative lookup: missing alias → ENOENT through the v3 errno
    // bridge.
    use tx_substrate::step_v3::{Errno as V3Errno, NoProgress};
    assert_eq!(
        <Devfs as FsOps>::lookup(&devfs, DEVFS_ROOT_OBJECT_ID, b"nope-v3", &guard),
        V3::<FsObjectId, NoProgress>::err(V3Errno::ENOENT)
    );
}

#[test]
fn devfs_v3_load_inode_meta_root_returns_directory_meta() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_tty_zones();

    use tx_subsystems::vfs::{FsOps, InodeKind};
    use tx_substrate::step_v3::StepOutcome as V3;

    let devfs = Devfs::new();
    let guard = tx_substrate::epoch::guard();

    let meta = match <Devfs as FsOps>::load_inode_meta(&devfs, DEVFS_ROOT_OBJECT_ID, &guard) {
        V3::Done(meta) => meta,
        other => panic!("v3 load_inode_meta(root): {other:?}"),
    };
    assert_eq!(meta.kind(), InodeKind::Directory);
}

#[test]
fn devfs_v3_fetch_page_returns_enosys() {
    // devfs is char-device-only — page-cache traffic does not flow
    // through it, so `fetch_page` surfaces `ENOSYS`. (Distinct from
    // tmpfs, whose `fetch_page` returns `Done(Frame)` for regular
    // files.)
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_tty_zones();

    use tx_subsystems::page_backed::{Frame, FsPageBacking};
    use tx_substrate::step_v3::{Errno as V3Errno, NoProgress, StepOutcome as V3};

    let devfs = Devfs::new();
    let guard = tx_substrate::epoch::guard();

    assert_eq!(
        <Devfs as FsPageBacking>::fetch_page(&devfs, DEVFS_ROOT_OBJECT_ID, 0, &guard),
        V3::<Frame, NoProgress>::err(V3Errno::ENOSYS)
    );
}

#[test]
fn devfs_readdir_yields_registered_aliases_and_terminates() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_tty_zones();

    let _ops = install_capturing_console();

    let guard = tx_substrate::epoch::guard();
    let devfs = Devfs::new();

    let mut cursor = DirCursor::START;
    let mut names: Vec<Vec<u8>> = Vec::new();
    loop {
        match <Devfs as FsOps>::readdir(&devfs, DEVFS_ROOT_OBJECT_ID, cursor, &guard) {
            V3Outcome::Done(Some((entry, next))) => {
                names.push(entry.name.as_bytes().to_vec());
                cursor = next;
            }
            V3Outcome::Done(None) => break,
            other => panic!("readdir failed: {other:?}"),
        }
    }

    // `register_hardware("console-hw", ...)` publishes a `console-hw`
    // alias; `register_console_alias("console", ...)` publishes a
    // `console` alias. Both should appear in readdir.
    assert!(
        names.iter().any(|n| n == b"console"),
        "readdir should yield console; got {names:?}"
    );
}
