//! Phase 3a tests: devfs as a standalone backend.
//!
//! Mount wiring (Phase 3b) is deliberately out of scope. These tests
//! construct a fake hardware TTY in-process via the existing
//! `tty::execution::register_hardware` + `register_console_alias`
//! seam, then exercise the devfs `FsOps` surface and
//! `open_console_for_init` directly.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use std::sync::Mutex;

use super::adapter::step_engine::{
    self as step_engine, guard, ByteProgress, Cap, Errno as V3Errno, StepOutcome as V3Outcome,
};
use tx_services::time::{
    platform::HalRtcDevice, DeadlineDomain, DeadlineNs, DeadlineRegistrarHandle,
    RtcDeviceOps as TimeRtcDeviceOps, TimeError, TimerRole, TimerTarget, TimerToken,
};
use tx_subsystems::device::{CharDeviceBinding, CharDeviceOps, DevT, RtcEventMask};
use tx_subsystems::execution::Guard;
use tx_subsystems::mount::{DevId, MountOptions, MountPayload, SourceLabel};
use tx_subsystems::tty::execution::{register_console_alias, register_hardware};
use tx_subsystems::vfs::{
    Credential, DirCursor, FsObjectId, FsOps, OpenFile, OpenFileFlags, RNodeBacking, StructPayload,
};

use super::{
    install_rtc_backend, open_console_for_init, publish_rtc_event_with_post,
    reset_rtc_backend_for_test, rtc_event_queue_for_test, rtc_event_source_id, Devfs,
    DEVFS_MISC_DIR_OBJECT_ID, DEVFS_ROOT_OBJECT_ID, DEVFS_RTC_OBJECT_ID, RTC_CHAR_BINDING,
};

static RTC_REF_POST_COUNT: AtomicUsize = AtomicUsize::new(0);

fn counting_rtc_ref_post(
    mailbox: &tx_substrate::wake::TaskMailbox,
    event: tx_substrate::wake::MailboxEvent,
) -> bool {
    RTC_REF_POST_COUNT.fetch_add(1, Ordering::SeqCst);
    mailbox.post(event)
}

fn explicit_rtc_ref_post(
    mailbox: &tx_substrate::wake::TaskMailbox,
    event: tx_substrate::wake::MailboxEvent,
) -> bool {
    mailbox.post(event)
}

struct TestDeadlineRoute {
    token: TimerToken,
    deadline_ns: DeadlineNs,
    role: TimerRole,
    target: TimerTarget,
}

struct TestDeadlineDomain {
    next_token: AtomicU64,
    routes: Mutex<Vec<TestDeadlineRoute>>,
}

impl TestDeadlineDomain {
    fn new() -> Self {
        Self {
            next_token: AtomicU64::new(1),
            routes: Mutex::new(Vec::new()),
        }
    }

    fn fire_due_with_post<F>(&self, now_ns: u64, mut post: F) -> usize
    where
        F: FnMut(&tx_substrate::wake::TaskMailbox, tx_substrate::wake::MailboxEvent) -> bool,
    {
        // The timer domain releases route ownership before device code publishes
        // readiness or wakes subscribers.
        let due = {
            let mut routes = self.routes.lock().expect("test deadline routes poisoned");
            let mut due = Vec::new();
            let mut pending = Vec::with_capacity(routes.len());
            for route in routes.drain(..) {
                if route.deadline_ns.raw() <= now_ns {
                    due.push(route);
                } else {
                    pending.push(route);
                }
            }
            *routes = pending;
            due
        };

        let fired = due.len();
        for route in due {
            assert_eq!(route.role, TimerRole::RtcAlarm);
            let TimerTarget::DeviceCallback(callback) = route.target else {
                panic!("RTC emulation must register a device callback route");
            };
            callback.fire();
            if let Some((queue, interests)) = callback.raw_queue_wake() {
                let _ = queue.fire_with_post(interests, |mailbox, event| post(mailbox, event));
            }
        }
        fired
    }
}

impl DeadlineDomain for TestDeadlineDomain {
    fn register_deadline(
        &self,
        deadline_ns: DeadlineNs,
        role: TimerRole,
        target: TimerTarget,
    ) -> Result<TimerToken, TimeError> {
        let token = TimerToken::new(self.next_token.fetch_add(1, Ordering::AcqRel));
        self.routes
            .lock()
            .expect("test deadline routes poisoned")
            .push(TestDeadlineRoute {
                token,
                deadline_ns,
                role,
                target,
            });
        Ok(token)
    }

    fn cancel_deadline(&self, token: TimerToken) -> bool {
        let mut routes = self.routes.lock().expect("test deadline routes poisoned");
        let Some(index) = routes.iter().position(|route| route.token == token) else {
            return false;
        };
        routes.swap_remove(index);
        true
    }

    fn rearm_deadline(&self, token: TimerToken, deadline_ns: DeadlineNs) -> bool {
        let mut routes = self.routes.lock().expect("test deadline routes poisoned");
        let Some(route) = routes.iter_mut().find(|route| route.token == token) else {
            return false;
        };
        route.deadline_ns = deadline_ns;
        true
    }
}

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
    tx_test_support::init_host();
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
    fn read(&self, _out: &mut [u8], _guard: &Guard<'_>) -> V3Outcome<usize, ByteProgress> {
        V3Outcome::Done(0)
    }

    fn write(&self, bytes: &[u8], _guard: &Guard<'_>) -> V3Outcome<usize, ByteProgress> {
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
    let guard = guard();
    let tty = match register_hardware("ttyS0", 0, binding, &guard) {
        V3Outcome::Done(tty) => tty,
        other => panic!("register_hardware failed: {other:?}"),
    };
    assert_eq!(
        register_console_alias("console", tty),
        step_engine::StepOutcome::Done(())
    );
    ops_static
}

struct TestRtcPlatform;

static TEST_RTC_NS: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(tx_services::time::DEFAULT_REALTIME_EPOCH_BASE_NS);
static TEST_RTC_SET_NS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static TEST_RTC_ALARM_NS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static TEST_RTC_ALARM_CLEAR_COUNT: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

impl tx_hal::PersistentClockIf for TestRtcPlatform {
    fn read_realtime_ns() -> Result<u64, tx_hal::PersistentClockError> {
        Ok(TEST_RTC_NS.load(core::sync::atomic::Ordering::Acquire))
    }

    fn set_realtime_ns(ns: u64) -> Result<(), tx_hal::PersistentClockError> {
        TEST_RTC_SET_NS.store(ns, core::sync::atomic::Ordering::Release);
        TEST_RTC_NS.store(ns, core::sync::atomic::Ordering::Release);
        Ok(())
    }

    fn set_wake_alarm_ns(ns: u64) -> Result<(), tx_hal::PersistentClockError> {
        TEST_RTC_ALARM_NS.store(ns, core::sync::atomic::Ordering::Release);
        Ok(())
    }

    fn clear_wake_alarm() -> Result<(), tx_hal::PersistentClockError> {
        TEST_RTC_ALARM_CLEAR_COUNT.fetch_add(1, core::sync::atomic::Ordering::AcqRel);
        Ok(())
    }
}

fn test_rtc_read_time_ns() -> Result<u64, TimeError> {
    HalRtcDevice::<TestRtcPlatform>::new().read_time_ns()
}

fn test_rtc_set_time_ns(ns: u64) -> Result<(), TimeError> {
    HalRtcDevice::<TestRtcPlatform>::new().set_time_ns(ns)
}

fn test_rtc_set_alarm_ns(ns: u64) -> Result<(), TimeError> {
    HalRtcDevice::<TestRtcPlatform>::new().set_alarm_ns(ns)
}

fn test_rtc_clear_alarm() -> Result<(), TimeError> {
    HalRtcDevice::<TestRtcPlatform>::new().clear_alarm()
}

fn install_test_rtc_backend() {
    install_rtc_backend(
        test_rtc_read_time_ns,
        test_rtc_set_time_ns,
        test_rtc_set_alarm_ns,
        test_rtc_clear_alarm,
    );
}

fn devfs_mount_payload() -> Cap<MountPayload> {
    MountPayload::new_cap(
        Devfs::fs_ops_arc(),
        Devfs::fs_page_backing_arc(),
        None,
        DevId::new(611),
        MountOptions::default(),
        "devfs-test",
        SourceLabel::Static("devfs-test"),
    )
    .expect("devfs mount payload")
}

// --- tests ----------------------------------------------------------------

#[test]
fn devfs_lookup_console_after_register_hardware_returns_tty_rnode() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_tty_zones();

    let _ops = install_capturing_console();

    let guard = guard();
    let devfs = Devfs::new();

    // FsOps::lookup over the devfs root yields a stable FsObjectId
    // for the alias.
    let obj_id = match <Devfs as FsOps>::lookup(&devfs, DEVFS_ROOT_OBJECT_ID, b"console", &guard) {
        V3Outcome::Done(id) => id,
        other => panic!("devfs.lookup(console) failed: {other:?}"),
    };

    // Materialise an RNode through the project layer (this is the same
    // shape the future VFS walker will produce on `step_open`).
    let rnode = match super::resolve_console_rnode(b"console") {
        V3Outcome::Done(rnode) => rnode,
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
fn devfs_lookup_ttys0_and_console_share_tty_identity() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_tty_zones();

    let _ops = install_capturing_console();

    let guard = guard();
    let devfs = Devfs::new();
    let mount = devfs_mount_payload();

    let console_id =
        match <Devfs as FsOps>::lookup(&devfs, DEVFS_ROOT_OBJECT_ID, b"console", &guard) {
            V3Outcome::Done(id) => id,
            other => panic!("devfs.lookup(console) failed: {other:?}"),
        };
    let ttys0_id = match <Devfs as FsOps>::lookup(&devfs, DEVFS_ROOT_OBJECT_ID, b"ttyS0", &guard) {
        V3Outcome::Done(id) => id,
        other => panic!("devfs.lookup(ttyS0) failed: {other:?}"),
    };

    let console_meta = match <Devfs as FsOps>::load_inode_meta(&devfs, console_id, &guard) {
        V3Outcome::Done(meta) => meta,
        other => panic!("devfs.load_inode_meta(console) failed: {other:?}"),
    };
    let ttys0_meta = match <Devfs as FsOps>::load_inode_meta(&devfs, ttys0_id, &guard) {
        V3Outcome::Done(meta) => meta,
        other => panic!("devfs.load_inode_meta(ttyS0) failed: {other:?}"),
    };

    let console_rnode =
        match <Devfs as FsOps>::materialise_rnode(&devfs, console_id, console_meta, &mount, &guard)
        {
            V3Outcome::Done(rnode) => rnode,
            other => panic!("devfs.materialise_rnode(console) failed: {other:?}"),
        };
    let ttys0_rnode =
        match <Devfs as FsOps>::materialise_rnode(&devfs, ttys0_id, ttys0_meta, &mount, &guard) {
            V3Outcome::Done(rnode) => rnode,
            other => panic!("devfs.materialise_rnode(ttyS0) failed: {other:?}"),
        };

    let console_tty = match console_rnode.backing() {
        RNodeBacking::StructBacked {
            payload: StructPayload::Tty(tty),
        } => tty.clone(),
        other => panic!("expected console StructBacked::Tty, got {other:?}"),
    };
    let ttys0_tty = match ttys0_rnode.backing() {
        RNodeBacking::StructBacked {
            payload: StructPayload::Tty(tty),
        } => tty.clone(),
        other => panic!("expected ttyS0 StructBacked::Tty, got {other:?}"),
    };
    assert_eq!(console_tty, ttys0_tty);

    let console_devt = super::devt_for_object_id(console_id).expect("console devt");
    assert_eq!((console_devt.major(), console_devt.minor()), (4, 64));
    let ttys0_devt = super::devt_for_object_id(ttys0_id).expect("ttyS0 devt");
    assert_eq!((ttys0_devt.major(), ttys0_devt.minor()), (4, 64));
}

#[test]
fn devfs_lookup_null_materialises_char_device() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_tty_zones();

    let guard = guard();
    let devfs = Devfs::new();

    let obj_id = match <Devfs as FsOps>::lookup(&devfs, DEVFS_ROOT_OBJECT_ID, b"null", &guard) {
        V3Outcome::Done(id) => id,
        other => panic!("devfs.lookup(null) failed: {other:?}"),
    };
    assert_ne!(obj_id, DEVFS_ROOT_OBJECT_ID);

    let meta = match <Devfs as FsOps>::load_inode_meta(&devfs, obj_id, &guard) {
        V3Outcome::Done(meta) => meta,
        other => panic!("devfs.load_inode_meta(null) failed: {other:?}"),
    };
    assert_eq!(meta.kind(), tx_subsystems::vfs::InodeKind::CharDevice);

    let mount = devfs_mount_payload();
    let rnode = match <Devfs as FsOps>::materialise_rnode(&devfs, obj_id, meta, &mount, &guard) {
        V3Outcome::Done(rnode) => rnode,
        other => panic!("devfs.materialise_rnode(null) failed: {other:?}"),
    };
    match rnode.backing() {
        RNodeBacking::StructBacked {
            payload: StructPayload::CharDevice(binding),
        } => assert_eq!(binding.name, "null"),
        other => panic!("expected StructBacked::CharDevice, got {other:?}"),
    }

    let file = OpenFile::new_cap(
        rnode,
        OpenFileFlags {
            read: true,
            write: true,
            append: false,
            cloexec: false,
            nonblocking: false,
            packet: false,
        },
    )
    .expect("null open file");
    let mut out = [0xaa; 4];
    assert_eq!(file.step_write(b"discarded", &guard), V3Outcome::Done(9));
    assert_eq!(file.step_read(&mut out, &guard), V3Outcome::Done(0));
    assert_eq!(out, [0xaa; 4]);
}

#[test]
fn devfs_lookup_misc_rtc_materialises_char_device() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_tty_zones();

    let guard = guard();
    let devfs = Devfs::new();

    let misc_id = match <Devfs as FsOps>::lookup(&devfs, DEVFS_ROOT_OBJECT_ID, b"misc", &guard) {
        V3Outcome::Done(id) => id,
        other => panic!("devfs.lookup(misc) failed: {other:?}"),
    };
    assert_eq!(misc_id, DEVFS_MISC_DIR_OBJECT_ID);

    let rtc_id = match <Devfs as FsOps>::lookup(&devfs, misc_id, b"rtc", &guard) {
        V3Outcome::Done(id) => id,
        other => panic!("devfs.lookup(misc/rtc) failed: {other:?}"),
    };
    assert_eq!(rtc_id, DEVFS_RTC_OBJECT_ID);

    let meta = match <Devfs as FsOps>::load_inode_meta(&devfs, rtc_id, &guard) {
        V3Outcome::Done(meta) => meta,
        other => panic!("devfs.load_inode_meta(rtc) failed: {other:?}"),
    };
    assert_eq!(meta.kind(), tx_subsystems::vfs::InodeKind::CharDevice);

    let mount = devfs_mount_payload();
    let rnode = match <Devfs as FsOps>::materialise_rnode(&devfs, rtc_id, meta, &mount, &guard) {
        V3Outcome::Done(rnode) => rnode,
        other => panic!("devfs.materialise_rnode(rtc) failed: {other:?}"),
    };
    match rnode.backing() {
        RNodeBacking::StructBacked {
            payload: StructPayload::CharDevice(binding),
        } => assert_eq!(binding.name, "rtc"),
        other => panic!("expected StructBacked::CharDevice, got {other:?}"),
    }
}

#[test]
fn devfs_rtc_ops_require_installed_persistent_backend() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_tty_zones();
    reset_rtc_backend_for_test();

    let guard = guard();
    let rtc_ops = RTC_CHAR_BINDING.ops.rtc_ops().expect("rtc typed ops");

    assert_eq!(
        rtc_ops.read_time(&guard),
        Err(tx_subsystems::device::RtcError::Unsupported)
    );
}

#[test]
fn devfs_rtc_ops_route_through_persistent_clock_backend() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_tty_zones();
    reset_rtc_backend_for_test();
    TEST_RTC_NS.store(
        tx_services::time::DEFAULT_REALTIME_EPOCH_BASE_NS,
        core::sync::atomic::Ordering::Release,
    );
    TEST_RTC_SET_NS.store(0, core::sync::atomic::Ordering::Release);
    TEST_RTC_ALARM_NS.store(0, core::sync::atomic::Ordering::Release);
    TEST_RTC_ALARM_CLEAR_COUNT.store(0, core::sync::atomic::Ordering::Release);
    install_test_rtc_backend();

    let guard = guard();
    let rtc_ops = RTC_CHAR_BINDING.ops.rtc_ops().expect("rtc typed ops");
    let rtc_time = rtc_ops.read_time(&guard).expect("persistent rtc read");
    assert_eq!(rtc_time.tm_mday, 23);
    assert_eq!(rtc_time.tm_mon, 4);
    assert_eq!(rtc_time.tm_year, 126);

    let updated =
        tx_subsystems::device::RtcTime::from_unix_seconds(1_800_000_000).expect("updated rtc time");
    rtc_ops
        .set_time(updated, &guard)
        .expect("persistent rtc set");
    assert_eq!(
        TEST_RTC_SET_NS.load(core::sync::atomic::Ordering::Acquire),
        1_800_000_000_000_000_000
    );

    let alarm_time =
        tx_subsystems::device::RtcTime::from_unix_seconds(1_800_000_123).expect("alarm time");
    rtc_ops
        .set_alarm(
            tx_subsystems::device::RtcAlarm {
                time: alarm_time,
                enabled: true,
                pending: false,
            },
            &guard,
        )
        .expect("persistent rtc alarm set");
    assert_eq!(
        TEST_RTC_ALARM_NS.load(core::sync::atomic::Ordering::Acquire),
        1_800_000_123_000_000_000
    );
    let alarm = rtc_ops
        .read_alarm(&guard)
        .expect("persistent rtc alarm read");
    assert_eq!(alarm.time, alarm_time);
    assert!(alarm.enabled);
    assert!(!alarm.pending);

    reset_rtc_backend_for_test();
}

#[test]
fn devfs_rtc_event_readiness_is_pending_state_and_read_consumes_record() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_tty_zones();
    reset_rtc_backend_for_test();
    install_test_rtc_backend();

    let guard = guard();
    let rtc_ops = RTC_CHAR_BINDING.ops.rtc_ops().expect("rtc typed ops");
    assert_eq!(rtc_ops.poll_events(&guard), Ok(RtcEventMask::empty()));

    publish_rtc_event_with_post(RtcEventMask::ALARM, explicit_rtc_ref_post);
    assert_eq!(rtc_ops.poll_events(&guard), Ok(RtcEventMask::ALARM));
    let alarm = rtc_ops.read_alarm(&guard).expect("alarm readable");
    assert!(alarm.pending);

    let mut record = [0u8; 8];
    assert_eq!(
        RTC_CHAR_BINDING.ops.read(&mut record, &guard),
        V3Outcome::Done(8)
    );
    let value = u64::from_ne_bytes(record);
    assert_ne!(value & 0x80, 0, "rtc read record carries RTC_IRQF");
    assert_ne!(value & 0x20, 0, "rtc read record carries RTC_AF");
    assert_eq!(rtc_ops.poll_events(&guard), Ok(RtcEventMask::empty()));
    let alarm = rtc_ops.read_alarm(&guard).expect("alarm readable");
    assert!(!alarm.pending);

    assert_eq!(
        RTC_CHAR_BINDING.ops.read(&mut record, &guard),
        V3Outcome::Err(V3Errno::EAGAIN)
    );

    reset_rtc_backend_for_test();
}

#[test]
fn devfs_rtc_event_with_post_uses_injected_mailbox_ref_post() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_tty_zones();
    reset_rtc_backend_for_test();
    install_test_rtc_backend();

    let _ = rtc_event_source_id();
    let queue = rtc_event_queue_for_test().expect("rtc event queue should be registered");
    let mailbox = Arc::new(tx_substrate::wake::TaskMailbox::new());
    let generation = mailbox.next_generation();
    let mut subscriber = queue.subscribe(
        super::RTC_EVENT_READABLE,
        Arc::downgrade(&mailbox),
        generation,
    );
    RTC_REF_POST_COUNT.store(0, Ordering::SeqCst);

    publish_rtc_event_with_post(RtcEventMask::ALARM, counting_rtc_ref_post);

    assert_eq!(
        RTC_REF_POST_COUNT.load(Ordering::SeqCst),
        1,
        "RTC event publication must use injected mailbox-ref post"
    );
    assert!(
        matches!(
            mailbox.poll(),
            Some(tx_substrate::wake::MailboxEvent::SourceFired {
                generation: fired,
                ..
            }) if fired == generation
        ),
        "RTC event publication must wake RTC waiters"
    );

    subscriber.unsubscribe();
    reset_rtc_backend_for_test();
}

#[test]
fn devfs_rtc_emulated_alarm_uses_domain_device_callback_route() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_tty_zones();
    reset_rtc_backend_for_test();

    let guard = guard();
    let rtc_ops = RTC_CHAR_BINDING.ops.rtc_ops().expect("rtc typed ops");
    let domain = Arc::new(TestDeadlineDomain::new());
    let registrar = DeadlineRegistrarHandle::from_domain(domain.clone());
    let alarm_time =
        tx_subsystems::device::RtcTime::from_unix_seconds(1_800_001_000).expect("alarm time");

    rtc_ops
        .set_alarm_with_emulation(
            tx_subsystems::device::RtcAlarm {
                time: alarm_time,
                enabled: true,
                pending: false,
            },
            &guard,
            Some(tx_subsystems::device::RtcAlarmEmulation::new(
                &registrar, 40,
            )),
        )
        .expect("rtc alarm emulation installed");

    let queue = rtc_event_queue_for_test().expect("rtc event queue should be registered");
    let mailbox = Arc::new(tx_substrate::wake::TaskMailbox::new());
    let generation = mailbox.next_generation();
    let mut subscriber = queue.subscribe(
        super::RTC_EVENT_READABLE,
        Arc::downgrade(&mailbox),
        generation,
    );
    RTC_REF_POST_COUNT.store(0, Ordering::SeqCst);

    assert_eq!(domain.fire_due_with_post(40, counting_rtc_ref_post), 1);

    assert_eq!(
        RTC_REF_POST_COUNT.load(Ordering::SeqCst),
        1,
        "emulated RTC alarm must use the deadline-domain mailbox-ref post"
    );
    assert_eq!(rtc_ops.poll_events(&guard), Ok(RtcEventMask::ALARM));
    assert!(
        matches!(
            mailbox.poll(),
            Some(tx_substrate::wake::MailboxEvent::SourceFired {
                generation: fired,
                ..
            }) if fired == generation
        ),
        "emulated RTC alarm must wake RTC RawQueue subscribers"
    );

    subscriber.unsubscribe();
    reset_rtc_backend_for_test();
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

    let guard = guard();
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

    let guard = guard();
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

    let guard = guard();
    let devfs = Devfs::new();
    let cred = Credential::root();

    // devfs nodes are kernel-owned; mode-bit mutation is not
    // supported. The override returns EROFS regardless of
    // privilege.
    assert_eq!(
        FsOps::chmod_inode(&devfs, DEVFS_ROOT_OBJECT_ID, 0o700, &cred, &guard),
        V3Outcome::err(V3Errno::EROFS)
    );
}

#[test]
fn devfs_chown_returns_erofs() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_tty_zones();

    let guard = guard();
    let devfs = Devfs::new();
    let cred = Credential::root();

    assert_eq!(
        FsOps::chown_inode(
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

    let guard = guard();
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

    use step_engine::StepOutcome as V3;
    use tx_subsystems::vfs::FsOps;

    let devfs = Devfs::new();
    let guard = guard();

    // Positive lookup: console alias is registered → `Done(id)` with a
    // non-root id.
    let id = match <Devfs as FsOps>::lookup(&devfs, DEVFS_ROOT_OBJECT_ID, b"console", &guard) {
        V3::Done(id) => id,
        other => panic!("v3 lookup(console): {other:?}"),
    };
    assert_ne!(id, DEVFS_ROOT_OBJECT_ID);

    // Negative lookup: missing alias → ENOENT through the v3 errno
    // bridge.
    use step_engine::{Errno as V3Errno, NoProgress};
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

    use step_engine::StepOutcome as V3;
    use tx_subsystems::vfs::{FsOps, InodeKind};

    let devfs = Devfs::new();
    let guard = guard();

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

    use step_engine::{Errno as V3Errno, NoProgress, StepOutcome as V3};
    use tx_subsystems::page_backed::{Frame, FsPageBacking};

    let devfs = Devfs::new();
    let guard = guard();

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

    let guard = guard();
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

    // `register_hardware("ttyS0", ...)` publishes the hardware entry;
    // `register_console_alias("console", ...)` publishes a second alias
    // for the same TTY. Both should appear in readdir.
    assert!(
        names.iter().any(|n| n == b"ttyS0"),
        "readdir should yield ttyS0; got {names:?}"
    );
    assert!(
        names.iter().any(|n| n == b"console"),
        "readdir should yield console; got {names:?}"
    );
}
