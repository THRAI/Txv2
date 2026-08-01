//! Topology tests for the process subsystem.
//!
//! These exercise the entity graph (Process / Thread / ProcessGroup /
//! Session) and the lifecycle steps (`bootstrap_init_process`,
//! `step_fork`, group exit, `step_setpgid`, `step_setsid`) without
//! signal state, credentials, rlimits, or fd-table coupling. The
//! identity/payload split is the primary subject under test: zombies
//! retain identity but drop payload.

use crate::cred::{sign_cred, Cred};
use crate::ipc::{posix_mq, sysv_msg, sysv_sem, sysv_shm};
use crate::mount::{
    DevId, MountFlags, MountId, MountIdentity, MountNamespace, MountOptions, MountPayload,
    SourceLabel,
};
use alloc::sync::Arc;

use crate::page_backed::{Frame as PageFrame, FsPageBacking};
use crate::process::adapter::step_engine::{
    guard as ebr_guard, sign, Cap, ScriptCtx, StepOp, StepOutcome,
};
use crate::process::adapter::wait_routing::{MailboxEvent, TaskMailbox};
use crate::process::execution::{
    init_process, reset_init_process_for_test, step_exit_group_with_signal_with_posts,
    BootstrapError, DupOp,
};
use crate::process::nsproxy::{PosixMqName, SysvKey};
use crate::process::numbers::{resolve_pid_number_as, PidName, PidNameKind};
use crate::process::structure::{
    reset_pid_counter_for_test, ExitStatus, Pgid, Pid, ProcessIdentity,
};
use crate::process::{
    bootstrap_init_process, step_chdir, step_chdir_with_mount, step_exit_group_with_posts,
    step_fork, step_fork_with_options, step_getcwd, step_set_mount_namespace, step_setpgid,
    step_setsid, step_waitpid_nohang, ChdirOutcome, ForkError, ForkOptions, SetpgidError,
    WaitError, WaitTarget,
};
use crate::signal::Signum;
use crate::test_support::EPOCH_TEST_LOCK;
use crate::thread_runtime::step_thread_exit;
use crate::thread_runtime::structure::{reset_tid_counter_for_test, ThreadIdentity};
use crate::vfs::{
    Credential, DEntry, DirCursor, DirEntry, FsObjectId, FsOps, InlineName, InodeKind, InodeMeta,
    RNode, RNodeBacking,
};
use crate::vm::{AddressSpace, TestPmap};
use crate::zones;
use core::future::Future;
use core::pin::Pin;
use core::ptr::null;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::time::Duration;

const NOOP_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
    |_| RawWaker::new(null(), &NOOP_WAKER_VTABLE),
    |_| {},
    |_| {},
    |_| {},
);

const TEST_COORDINATION_TIMEOUT: Duration = Duration::from_secs(5);

struct TestPauseController {
    reached: Receiver<()>,
    release: Option<SyncSender<()>>,
}

struct TestPauseWorker {
    reached: SyncSender<()>,
    release: Receiver<()>,
}

fn bounded_test_pause() -> (TestPauseController, TestPauseWorker) {
    let (reached_tx, reached_rx) = sync_channel(1);
    let (release_tx, release_rx) = sync_channel(1);
    (
        TestPauseController {
            reached: reached_rx,
            release: Some(release_tx),
        },
        TestPauseWorker {
            reached: reached_tx,
            release: release_rx,
        },
    )
}

impl TestPauseController {
    fn wait_until_paused(&self) {
        self.reached
            .recv_timeout(TEST_COORDINATION_TIMEOUT)
            .expect("worker reaches bounded test pause");
    }

    fn release(mut self) {
        let release = self.release.take().expect("test pause release is one-shot");
        let _ = release.try_send(());
    }
}

impl Drop for TestPauseController {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.try_send(());
        }
    }
}

impl TestPauseWorker {
    fn pause(self) {
        self.reached
            .send(())
            .expect("test controller receives pause notification");
        self.release
            .recv_timeout(TEST_COORDINATION_TIMEOUT)
            .expect("test controller releases paused worker");
    }
}

#[test]
fn bounded_test_pause_release_tolerates_worker_disconnect() {
    let (controller, worker) = bounded_test_pause();
    let TestPauseWorker { reached, release } = worker;
    reached.send(()).expect("controller observes worker");
    drop(release);

    controller.wait_until_paused();
    controller.release();
}

fn direct_process_sem_ref_post(mailbox: &TaskMailbox, event: MailboxEvent) -> bool {
    mailbox.post(event)
}

fn direct_process_task_post(weak: alloc::sync::Weak<TaskMailbox>, event: MailboxEvent) {
    let Some(mailbox) = weak.upgrade() else {
        return;
    };
    let _ = mailbox.post(event);
}

fn finish_process_group_for_test(process: &Cap<ProcessIdentity>, status: ExitStatus) {
    step_exit_group_with_posts(
        process,
        status,
        direct_process_task_post,
        direct_process_sem_ref_post,
    );
}

fn finish_process_group_with_signal_for_test(process: &Cap<ProcessIdentity>, sig: Signum) {
    step_exit_group_with_signal_with_posts(
        process,
        sig,
        direct_process_task_post,
        direct_process_sem_ref_post,
    );
}

fn noop_waker() -> Waker {
    unsafe { Waker::from_raw(RawWaker::new(null(), &NOOP_WAKER_VTABLE)) }
}

fn block_on<F: Future>(mut future: F) -> F::Output {
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    // Safety: the future is stack-pinned for this helper call.
    let mut pinned = unsafe { Pin::new_unchecked(&mut future) };
    for _ in 0..1024 {
        match pinned.as_mut().poll(&mut cx) {
            Poll::Ready(out) => return out,
            Poll::Pending => {}
        }
    }
    panic!("process test block_on: future did not resolve in 1024 polls");
}

fn setup() -> std::sync::MutexGuard<'static, ()> {
    let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    tx_test_support::init_host();
    let _ = zones::register_all();
    tx_test_support::drain_to_quiescence();
    reset_pid_counter_for_test();
    reset_tid_counter_for_test();
    reset_init_process_for_test();
    guard
}

fn fresh_aspace() -> Cap<AddressSpace> {
    AddressSpace::new_cap_for_platform::<TestPmap>().expect("fresh aspace")
}

fn bootstrap() -> Cap<ProcessIdentity> {
    bootstrap_init_process(fresh_aspace()).expect("bootstrap init")
}

#[cfg(all(tx_ds_metrics, tx_ds_metrics_process))]
#[test]
fn process_ds_metrics_declares_identity_tree_method_names() {
    let names = crate::process::ds_metrics::PROCESS_DS_METHOD_NAMES;

    assert!(names.contains(&b"debug.ds.process.pid_namespace.register_pid".as_slice()));
    assert!(names.contains(&b"debug.ds.process.pid_namespace.resolve_pid_number_as".as_slice()));
    assert!(names.contains(&b"debug.ds.process.pid_namespace.unregister_pid_number".as_slice()));
    assert!(names.contains(&b"debug.ds.process.children.snapshot".as_slice()));
    assert!(names.contains(&b"debug.ds.process.threads.snapshot".as_slice()));
}

#[test]
fn process_lock_service_declares_high_stake_payload_phase_names() {
    let names = crate::process::execution::PROCESS_LOCK_SERVICE_TRACE_NAMES;

    assert!(names.contains(
        &b"debug.lock_service.process.payload.exit_group.shm_detach.duration_ns".as_slice()
    ));
    assert!(names.contains(
        &b"debug.lock_service.process.payload.exit_group.drain_fds.duration_ns".as_slice()
    ));
    assert!(names.contains(
        &b"debug.lock_service.process.payload.exit_group.threads_drain.duration_ns".as_slice()
    ));
    assert!(names.contains(
        &b"debug.lock_service.process.payload.exit_group.zombify_threads.duration_ns".as_slice()
    ));
    assert!(names.contains(
        &b"debug.lock_service.process.payload.exit_group.drop_drained.duration_ns".as_slice()
    ));
    assert!(names.contains(
        &b"debug.lock_service.process.payload.exit_group.payload_drop.duration_ns".as_slice()
    ));
    assert!(names.contains(
        &b"debug.lock_service.process.payload.process_exit.shm_detach.duration_ns".as_slice()
    ));
    assert!(names.contains(
        &b"debug.lock_service.process.payload.process_exit.drain_fds.duration_ns".as_slice()
    ));
    assert!(names.contains(
        &b"debug.lock_service.process.payload.process_exit.drop_closed_fds.duration_ns".as_slice()
    ));
    assert!(names.contains(
        &b"debug.lock_service.process.payload.process_exit.payload_drop.duration_ns".as_slice()
    ));
    assert!(names.contains(
        &b"debug.lock_service.process.payload.thread_exit.threads_detach.duration_ns".as_slice()
    ));
    assert!(names.contains(
        &b"debug.lock_service.process.payload.thread_exit.thread_count.duration_ns".as_slice()
    ));
    assert!(names.contains(
        &b"debug.lock_service.process.payload.thread_exit.group_exit.duration_ns".as_slice()
    ));
    assert!(names
        .contains(&b"debug.lock_service.process.payload.robust.head_reads.duration_ns".as_slice()));
    assert!(names
        .contains(&b"debug.lock_service.process.payload.robust.entries.duration_ns".as_slice()));
    assert!(names
        .contains(&b"debug.lock_service.process.payload.robust.pending.duration_ns".as_slice()));
    assert!(names.contains(&b"debug.lock_service.process.payload.robust.entry_count".as_slice()));
}

fn source_function_body<'a>(src: &'a str, name: &str) -> &'a str {
    let start = src.find(name).expect("function name present");
    let open = src[start..]
        .find('{')
        .map(|idx| start + idx)
        .expect("function body opens");
    let mut depth = 0usize;
    for (offset, ch) in src[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &src[open + 1..open + offset];
                }
            }
            _ => {}
        }
    }
    panic!("function body closes");
}

fn source_between<'a>(src: &'a str, start_pat: &str, end_pat: &str) -> &'a str {
    let start = src.find(start_pat).expect("start pattern present");
    let end = src[start..]
        .find(end_pat)
        .map(|idx| start + idx)
        .expect("end pattern present");
    &src[start..end]
}

#[test]
fn exit_group_payload_slot_guard_only_detaches_state() {
    let body = source_function_body(
        include_str!("execution.rs"),
        "pub fn step_exit_group_with_posts",
    );
    let guard_region = source_between(
        body,
        "let payload_guard = process.payload.lock();",
        "if let Some(aspace) = exit_aspace",
    );

    for forbidden in [
        "close_socket_files_for_process_exit",
        "notify_thread_exit_userspace_in_aspace",
        "set_thread_zombie_with_post",
        "unregister_tid_number",
        "drop(closed_fds)",
        "drop(drained)",
    ] {
        assert!(
            !guard_region.contains(forbidden),
            "{forbidden} must run after dropping process.payload guard"
        );
    }
}

#[test]
fn process_exit_payload_slot_guard_only_detaches_state() {
    let body = source_function_body(include_str!("execution.rs"), "fn step_process_exit_inner");
    let guard_region = source_between(
        body,
        "let payload_guard = process.payload.lock();",
        "if let Some(aspace) = exit_aspace",
    );

    for forbidden in ["close_socket_files_for_process_exit", "drop(closed_fds)"] {
        assert!(
            !guard_region.contains(forbidden),
            "{forbidden} must run after dropping process.payload guard"
        );
    }
}

struct NullMountFs;

#[derive(Default)]
struct RecordingFsyncBacking {
    fsyncs: AtomicUsize,
}

impl FsPageBacking for RecordingFsyncBacking {
    fn fetch_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _guard: &crate::execution::Guard<'_>,
    ) -> StepOutcome<PageFrame, crate::process::adapter::step_engine::NoProgress> {
        unreachable!("exit flush test has no resident pages")
    }

    fn flush_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _frame: &PageFrame,
        _guard: &crate::execution::Guard<'_>,
    ) -> StepOutcome<(), crate::process::adapter::step_engine::NoProgress> {
        StepOutcome::done(())
    }

    fn truncate(
        &self,
        _fs_object_id: FsObjectId,
        _new_size: u64,
        _guard: &crate::execution::Guard<'_>,
    ) -> StepOutcome<(), crate::process::adapter::step_engine::NoProgress> {
        StepOutcome::done(())
    }

    fn fsync_file(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &crate::execution::Guard<'_>,
    ) -> StepOutcome<(), crate::process::adapter::step_engine::NoProgress> {
        self.fsyncs.fetch_add(1, Ordering::AcqRel);
        StepOutcome::done(())
    }
}

fn recording_page_backed_open_file() -> (Cap<crate::vfs::OpenFile>, Arc<RecordingFsyncBacking>) {
    use crate::mount::MountPayloadPin;
    use crate::page_backed::PageContainer;
    use crate::process::adapter::step_engine::PayloadCap;
    use crate::vfs::OpenFileFlags;

    let backing = Arc::new(RecordingFsyncBacking::default());
    let mount = MountPayload::new_cap(
        Arc::new(NullMountFs),
        backing.clone(),
        None,
        DevId::new(101),
        MountOptions::default(),
        "recording-fsync",
        SourceLabel::Static("recording-fsync"),
    )
    .expect("recording fsync mount payload");
    let pc = PageContainer::new_file_cap(
        MountPayloadPin::acquire(&PayloadCap::from_cap(mount)),
        FsObjectId::new(2),
        0,
    )
    .expect("recording file page container");
    let rnode = RNode::new_cap(
        FsObjectId::new(2),
        InodeMeta::new(InodeKind::Regular, 0o100644),
        RNodeBacking::PageBacked { pc },
    )
    .expect("recording file rnode");
    let file = crate::vfs::OpenFile::new_cap(rnode, OpenFileFlags::default())
        .expect("recording open file");
    (file, backing)
}

impl FsOps for NullMountFs {
    fn lookup(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _guard: &crate::execution::Guard<'_>,
    ) -> StepOutcome<FsObjectId, crate::process::adapter::step_engine::NoProgress> {
        unreachable!("process namespace tests never walk the dummy mount")
    }

    fn load_inode_meta(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &crate::execution::Guard<'_>,
    ) -> StepOutcome<InodeMeta, crate::process::adapter::step_engine::NoProgress> {
        unreachable!("process namespace tests never walk the dummy mount")
    }

    fn serialize_inode_meta(
        &self,
        _fs_object_id: FsObjectId,
        _meta: &InodeMeta,
        _guard: &crate::execution::Guard<'_>,
    ) -> StepOutcome<(), crate::process::adapter::step_engine::NoProgress> {
        unreachable!("process namespace tests never walk the dummy mount")
    }

    fn create_inode(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &crate::execution::Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), crate::process::adapter::step_engine::NoProgress>
    {
        unreachable!("process namespace tests never walk the dummy mount")
    }

    fn unlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &crate::execution::Guard<'_>,
    ) -> StepOutcome<(), crate::process::adapter::step_engine::NoProgress> {
        unreachable!("process namespace tests never walk the dummy mount")
    }

    fn rename(
        &self,
        _old_parent: FsObjectId,
        _old_name: &[u8],
        _new_parent: FsObjectId,
        _new_name: &[u8],
        _guard: &crate::execution::Guard<'_>,
    ) -> StepOutcome<(), crate::process::adapter::step_engine::NoProgress> {
        unreachable!("process namespace tests never walk the dummy mount")
    }

    fn link(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &crate::execution::Guard<'_>,
    ) -> StepOutcome<(), crate::process::adapter::step_engine::NoProgress> {
        unreachable!("process namespace tests never walk the dummy mount")
    }

    fn mkdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &crate::execution::Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), crate::process::adapter::step_engine::NoProgress>
    {
        unreachable!("process namespace tests never walk the dummy mount")
    }

    fn rmdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &crate::execution::Guard<'_>,
    ) -> StepOutcome<(), crate::process::adapter::step_engine::NoProgress> {
        unreachable!("process namespace tests never walk the dummy mount")
    }

    fn symlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _link_target: &[u8],
        _cred: &Credential,
        _guard: &crate::execution::Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), crate::process::adapter::step_engine::NoProgress>
    {
        unreachable!("process namespace tests never walk the dummy mount")
    }

    fn readdir(
        &self,
        _fs_object_id: FsObjectId,
        _cursor: DirCursor,
        _guard: &crate::execution::Guard<'_>,
    ) -> StepOutcome<Option<(DirEntry, DirCursor)>, crate::process::adapter::step_engine::NoProgress>
    {
        unreachable!("process namespace tests never walk the dummy mount")
    }

    fn destroy_inode(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &crate::execution::Guard<'_>,
    ) -> StepOutcome<(), crate::process::adapter::step_engine::NoProgress> {
        unreachable!("process namespace tests never walk the dummy mount")
    }
}

impl FsPageBacking for NullMountFs {
    fn fetch_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _guard: &crate::execution::Guard<'_>,
    ) -> StepOutcome<PageFrame, crate::process::adapter::step_engine::NoProgress> {
        unreachable!("process namespace tests never page the dummy mount")
    }

    fn flush_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _frame: &PageFrame,
        _guard: &crate::execution::Guard<'_>,
    ) -> StepOutcome<(), crate::process::adapter::step_engine::NoProgress> {
        unreachable!("process namespace tests never page the dummy mount")
    }

    fn truncate(
        &self,
        _fs_object_id: FsObjectId,
        _new_size: u64,
        _guard: &crate::execution::Guard<'_>,
    ) -> StepOutcome<(), crate::process::adapter::step_engine::NoProgress> {
        unreachable!("process namespace tests never page the dummy mount")
    }

    fn fsync_file(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &crate::execution::Guard<'_>,
    ) -> StepOutcome<(), crate::process::adapter::step_engine::NoProgress> {
        unreachable!("process namespace tests never page the dummy mount")
    }
}

fn fresh_mount_namespace() -> Cap<MountNamespace> {
    let fs = Arc::new(NullMountFs);
    let payload = MountPayload::new_cap(
        fs.clone(),
        fs,
        None,
        DevId::new(100),
        MountOptions::default(),
        "nullfs",
        SourceLabel::Static("nullfs"),
    )
    .expect("dummy mount payload");
    let root = RNode::new_cap_in_mount(
        FsObjectId::ROOT,
        InodeMeta::new(InodeKind::Directory, 0o040755),
        RNodeBacking::Directory,
        &payload,
    )
    .expect("dummy root rnode");
    let root_mount = MountIdentity::new_cap(
        MountId::new(100),
        None,
        root,
        None,
        payload,
        MountFlags::empty(),
    )
    .expect("dummy root mount");
    MountNamespace::new_cap(root_mount).expect("dummy mount namespace")
}

fn first_thread(proc_cap: &Cap<ProcessIdentity>) -> Cap<ThreadIdentity> {
    let payload_guard = proc_cap.payload.lock();
    let payload = payload_guard.as_ref().expect("alive");
    let threads = payload.threads.snapshot();
    threads[0].clone()
}

#[test]
fn subject_identity_signal_pending_checks_authoritative_pending_state() {
    let _g = setup();
    let proc = bootstrap();
    let leader = first_thread(&proc);

    crate::thread_runtime::execution::post_signal_with_post(
        &leader,
        Signum::SIGTERM,
        tx_substrate::wake::SignalRouting::ProcessDirected,
        None,
        |weak, event| {
            let Some(mailbox) = weak.upgrade() else {
                return;
            };
            let _ = mailbox.post(event);
        },
    );
    assert!(
        <ProcessIdentity as crate::process::adapter::step_engine::SubjectIdentity>::
            thread_deliverable_signal_pending(&leader),
        "posted signal should be observable through the subject predicate"
    );

    leader
        .payload_cap()
        .expect("leader payload")
        .pending()
        .clear(Signum::SIGTERM);

    assert!(
        !<ProcessIdentity as crate::process::adapter::step_engine::SubjectIdentity>::
            thread_deliverable_signal_pending(&leader),
        "predicate must follow the pending queues even if the cached summary bit is stale"
    );
}

#[test]
fn bootstrap_init_creates_pid_1_with_session_and_pgrp() {
    let _g = setup();
    let init = bootstrap();

    assert_eq!(init.pid, Pid::INIT);
    assert_eq!(init.parent_pid(), Pid::RESERVED);
    assert!(init.parent_cap().is_none());
    assert!(!init.is_zombie());
    assert_eq!(init.live_thread_count(), 1);

    let pgrp = init.pgrp_cap();
    assert_eq!(pgrp.pgid, Pgid(Pid::INIT.0));
    let session = pgrp.session_cap();
    assert_eq!(session.sid.0, Pid::INIT.0);
    assert!(!session.has_controlling_tty());
}

#[test]
fn fork_creates_child_with_leader_thread_and_inherits_pgrp() {
    let _g = setup();
    let parent = bootstrap();

    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");

    assert_ne!(child.pid, parent.pid);
    assert_eq!(child.parent_pid(), parent.pid);
    assert_eq!(child.parent_cap().expect("parent retained").pid, parent.pid);
    assert!(!child.is_zombie());
    assert_eq!(child.live_thread_count(), 1);

    // Child inherits parent's pgrp.
    assert_eq!(child.pgrp_cap().pgid, parent.pgrp_cap().pgid);
}

#[test]
fn fork_with_clone_newipc_publishes_fresh_empty_ipc_namespace() {
    let _g = setup();
    let parent = bootstrap();
    let parent_nsproxy = parent.nsproxy_cap().expect("parent nsproxy");

    {
        let mut limits = parent_nsproxy.ipc_ns.limits.lock();
        limits.mq_maxmsg = 17;
        limits.msgmni = 23;
    }
    let ipc_cred = sign_cred(Cred::root()).expect("ipc cred cap");
    let sem_identity = sysv_sem::structure::register_sem(
        Some(SysvKey::new(0x10)),
        ipc_cred.clone(),
        1,
        sysv_shm::structure::IpcPerm::new(0o600),
        0,
        0,
    )
    .expect("sem identity");
    parent_nsproxy
        .ipc_ns
        .sysv_sem
        .lock()
        .insert(SysvKey::new(0x10), sem_identity);
    let shm_identity = sysv_shm::structure::register_shm(
        Some(SysvKey::new(0x20)),
        ipc_cred.clone(),
        4096,
        sysv_shm::structure::IpcPerm::new(0o600),
        0,
        0,
    )
    .expect("shm identity");
    parent_nsproxy
        .ipc_ns
        .sysv_shm
        .lock()
        .insert(SysvKey::new(0x20), shm_identity);
    let msg_identity = sysv_msg::structure::register_msg(
        Some(SysvKey::new(0x30)),
        ipc_cred.clone(),
        sysv_shm::structure::IpcPerm::new(0o600),
        0,
        0,
        16,
        16,
    )
    .expect("msg identity");
    parent_nsproxy
        .ipc_ns
        .sysv_msg
        .lock()
        .insert(SysvKey::new(0x30), msg_identity);
    let mq_cred = sign_cred(Cred::root()).expect("mq cred cap");
    let (_mqid, mq_identity) = posix_mq::structure::register_mq(
        PosixMqName::new(b"/parent"),
        mq_cred,
        sysv_shm::structure::IpcPerm::new(0o600),
        404,
        2,
        16,
    )
    .expect("mq identity");
    parent_nsproxy
        .ipc_ns
        .posix_mq
        .lock()
        .insert(PosixMqName::new(b"/parent"), mq_identity);

    let child = step_fork_with_options::<TestPmap>(
        &parent,
        ForkOptions {
            clone_newipc: true,
            ..ForkOptions::default()
        },
    )
    .expect("fork with CLONE_NEWIPC");
    let child_nsproxy = child.nsproxy_cap().expect("child nsproxy");

    assert_ne!(
        parent_nsproxy.key().raw(),
        child_nsproxy.key().raw(),
        "CLONE_NEWIPC must publish a replacement nsproxy bundle"
    );
    assert_ne!(
        parent_nsproxy.ipc_ns.key().raw(),
        child_nsproxy.ipc_ns.key().raw(),
        "CLONE_NEWIPC must create a fresh IPC namespace"
    );
    assert_eq!(
        parent_nsproxy.pid_ns.key().raw(),
        child_nsproxy.pid_ns.key().raw(),
        "non-IPC namespaces stay shared in this slice"
    );

    let child_limits = *child_nsproxy.ipc_ns.limits.lock();
    assert_eq!(child_limits.mq_maxmsg, 17);
    assert_eq!(child_limits.msgmni, 23);
    assert!(child_nsproxy.ipc_ns.sysv_sem.lock().is_empty());
    assert!(child_nsproxy.ipc_ns.sysv_shm.lock().is_empty());
    assert!(child_nsproxy.ipc_ns.sysv_msg.lock().is_empty());
    assert!(child_nsproxy.ipc_ns.posix_mq.lock().is_empty());

    assert_eq!(parent_nsproxy.ipc_ns.sysv_sem.lock().len(), 1);
    assert_eq!(parent_nsproxy.ipc_ns.sysv_shm.lock().len(), 1);
    assert_eq!(parent_nsproxy.ipc_ns.sysv_msg.lock().len(), 1);
    assert_eq!(parent_nsproxy.ipc_ns.posix_mq.lock().len(), 1);
}

#[test]
fn fork_inherits_mount_namespace_from_nsproxy_bundle() {
    let _g = setup();
    let parent = bootstrap();
    let parent_mnt_ns = fresh_mount_namespace();
    let parent_nsproxy = parent.nsproxy_cap().expect("parent nsproxy");
    let replacement =
        crate::process::nsproxy::clone_nsproxy_with_mount_namespace(&parent_nsproxy, parent_mnt_ns)
            .expect("replacement nsproxy");

    {
        let payload_guard = parent.payload.lock();
        let payload = payload_guard.as_ref().expect("parent payload");
        let _old = payload.replace_nsproxy(replacement);
    }

    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");
    let parent_nsproxy = parent.nsproxy_cap().expect("parent nsproxy");
    let child_nsproxy = child.nsproxy_cap().expect("child nsproxy");

    assert_eq!(
        parent_nsproxy
            .mnt_ns
            .as_ref()
            .expect("parent mnt ns")
            .key()
            .raw(),
        child_nsproxy
            .mnt_ns
            .as_ref()
            .expect("child mnt ns")
            .key()
            .raw(),
        "plain fork must inherit the mount namespace cap"
    );
}

#[test]
fn mounted_cwd_binding_is_atomic_across_fork_and_survives_setns() {
    let _g = setup();
    let process = bootstrap();
    let namespace = fresh_mount_namespace();
    step_set_mount_namespace(&process, namespace.clone()).expect("install mount namespace");
    let root_mount = namespace.root().clone();
    let root_dentry = namespace.root_dentry();

    match step_chdir_with_mount(&process, root_dentry.clone(), root_mount.clone()) {
        ChdirOutcome::Replaced { .. } => {}
        ChdirOutcome::ZombieIgnored => panic!("live process rejected mounted cwd"),
    }
    let parent_cwd = process.cwd_binding().expect("mounted parent cwd");
    assert_eq!(parent_cwd.dentry.key(), root_dentry.key());
    assert_eq!(parent_cwd.mount.key(), root_mount.key());

    let child = step_fork::<TestPmap>(&process, false, false).expect("fork child");
    let child_cwd = child.cwd_binding().expect("forked mounted cwd");
    assert_eq!(child_cwd.dentry.key(), parent_cwd.dentry.key());
    assert_eq!(child_cwd.mount.key(), parent_cwd.mount.key());

    let replacement_namespace = fresh_mount_namespace();
    step_set_mount_namespace(&process, replacement_namespace).expect("setns replacement");
    let origin_held = process.cwd_binding().expect("origin-held cwd after setns");
    assert_eq!(origin_held.dentry.key(), parent_cwd.dentry.key());
    assert_eq!(origin_held.mount.key(), parent_cwd.mount.key());
}

#[test]
fn fork_clones_address_space_into_distinct_cap() {
    let _g = setup();
    let parent = bootstrap();
    let parent_aspace = parent.aspace_cap().expect("parent live");

    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");
    let child_aspace = child.aspace_cap().expect("child live");

    // The Cap keys must differ — child has its own address space slot.
    assert_ne!(parent_aspace.key(), child_aspace.key());
}

#[test]
fn fork_registers_child_in_parent_pgrp_member_list() {
    let _g = setup();
    let parent = bootstrap();
    let pgrp = parent.pgrp_cap();
    let before = pgrp.member_slot_count();

    let _child = step_fork::<TestPmap>(&parent, false, false).expect("fork");

    assert_eq!(pgrp.member_slot_count(), before + 1);
}

#[test]
fn fork_on_zombie_parent_returns_parent_zombie() {
    let _g = setup();
    let parent = bootstrap();
    finish_process_group_for_test(&parent, ExitStatus::Exited(0));
    assert!(parent.is_zombie());

    let result = step_fork::<TestPmap>(&parent, false, false);
    assert!(matches!(result, Err(ForkError::ParentZombie)));
}

#[test]
fn last_thread_exit_zombifies_process_keeps_identity() {
    let _g = setup();
    let proc_cap = bootstrap();
    let leader = first_thread(&proc_cap);

    step_thread_exit(leader, 7);

    assert!(proc_cap.is_zombie());
    assert_eq!(proc_cap.exit_status(), Some(ExitStatus::Exited(7)));
    assert_eq!(proc_cap.pid, Pid::INIT);
}

#[test]
fn last_thread_exit_detaches_live_sysv_shm_mappings() {
    let _g = setup();
    let proc_cap = bootstrap();
    let aspace = proc_cap.aspace_cap().expect("live aspace");
    let ns = crate::process::nsproxy::sign_init_nsproxy().expect("nsproxy cap");
    let cred = sign_cred(Cred::root()).expect("root cred cap");
    let shmid = sysv_shm::execution::step_shmget(
        sysv_shm::execution::IPC_PRIVATE,
        4096,
        sysv_shm::execution::IPC_CREAT | 0o600,
        &cred,
        &ns,
    )
    .expect("shmget private");
    let addr = block_on(sysv_shm::execution::script_shmat(
        shmid, 0, 0, &cred, &aspace,
    ))
    .expect("shmat");
    assert!(aspace.lookup(crate::vm::UserVirtAddr(addr)).is_some());

    let leader = first_thread(&proc_cap);
    step_thread_exit(leader, 7);

    assert!(proc_cap.is_zombie());
    assert!(aspace.lookup(crate::vm::UserVirtAddr(addr)).is_none());
    let stat =
        match sysv_shm::execution::step_shmctl(shmid, sysv_shm::execution::IPC_STAT, None, &cred)
            .expect("stat after exit")
        {
            sysv_shm::execution::ShmCtlResult::Stat(info) => info,
            other => panic!("expected Stat, got {other:?}"),
        };
    assert_eq!(stat.attach_count, 0);
    sysv_shm::execution::step_shmctl(shmid, sysv_shm::execution::IPC_RMID, None, &cred)
        .expect("rmid");
}

#[test]
fn exit_group_zombifies_process_at_once_and_records_status() {
    let _g = setup();
    let proc_cap = bootstrap();

    finish_process_group_for_test(&proc_cap, ExitStatus::Exited(42));

    assert!(proc_cap.is_zombie());
    assert_eq!(proc_cap.exit_status(), Some(ExitStatus::Exited(42)));
    assert_eq!(proc_cap.live_thread_count(), 0);
}

#[test]
fn exit_group_flushes_page_backed_files_drained_from_fd_table() {
    let _g = setup();
    let proc_cap = bootstrap();
    let (file, backing) = recording_page_backed_open_file();
    proc_cap.set_fd(1, Some(file));

    finish_process_group_for_test(&proc_cap, ExitStatus::Exited(0));

    assert_eq!(backing.fsyncs.load(Ordering::Acquire), 1);
}

#[test]
fn last_thread_exit_flushes_page_backed_files_drained_from_fd_table() {
    let _g = setup();
    let proc_cap = bootstrap();
    let (file, backing) = recording_page_backed_open_file();
    proc_cap.set_fd(1, Some(file));
    let leader = first_thread(&proc_cap);

    step_thread_exit(leader, 0);

    assert_eq!(backing.fsyncs.load(Ordering::Acquire), 1);
}

#[test]
fn exit_group_detaches_live_sysv_shm_mappings() {
    let _g = setup();
    let proc_cap = bootstrap();
    let aspace = proc_cap.aspace_cap().expect("live aspace");
    let ns = crate::process::nsproxy::sign_init_nsproxy().expect("nsproxy cap");
    let cred = sign_cred(Cred::root()).expect("root cred cap");
    let shmid = sysv_shm::execution::step_shmget(
        sysv_shm::execution::IPC_PRIVATE,
        4096,
        sysv_shm::execution::IPC_CREAT | 0o600,
        &cred,
        &ns,
    )
    .expect("shmget private");
    let addr = block_on(sysv_shm::execution::script_shmat(
        shmid, 0, 0, &cred, &aspace,
    ))
    .expect("shmat");
    assert!(aspace.lookup(crate::vm::UserVirtAddr(addr)).is_some());

    finish_process_group_for_test(&proc_cap, ExitStatus::Exited(42));

    assert!(proc_cap.is_zombie());
    assert!(aspace.lookup(crate::vm::UserVirtAddr(addr)).is_none());
    let stat =
        match sysv_shm::execution::step_shmctl(shmid, sysv_shm::execution::IPC_STAT, None, &cred)
            .expect("stat after exit")
        {
            sysv_shm::execution::ShmCtlResult::Stat(info) => info,
            other => panic!("expected Stat, got {other:?}"),
        };
    assert_eq!(stat.attach_count, 0);
    sysv_shm::execution::step_shmctl(shmid, sysv_shm::execution::IPC_RMID, None, &cred)
        .expect("rmid");
}

#[test]
fn last_thread_exit_applies_sysv_sem_undo_adjustments() {
    let _g = setup();
    let proc_cap = bootstrap();
    let ns = proc_cap.nsproxy_cap().expect("nsproxy cap");
    let cred = sign_cred(Cred::root()).expect("root cred cap");
    let semid = sysv_sem::execution::step_semget(
        sysv_shm::execution::IPC_PRIVATE,
        1,
        sysv_shm::execution::IPC_CREAT | 0o600,
        &cred,
        &ns,
    )
    .expect("semget private");
    sysv_sem::execution::step_semctl_with_post(
        semid,
        0,
        sysv_sem::execution::SETVAL,
        sysv_sem::execution::SemCtlArg::Val(2),
        &cred,
        None,
        direct_process_sem_ref_post,
    )
    .expect("SETVAL");
    sysv_sem::execution::step_semop_with_post(
        semid,
        &[sysv_sem::structure::SemBuf {
            sem_num: 0,
            sem_op: -2,
            sem_flg: sysv_sem::structure::sem_flg::SEM_UNDO,
        }],
        &cred,
        &proc_cap,
        direct_process_sem_ref_post,
    )
    .expect("semop SEM_UNDO");

    let leader = first_thread(&proc_cap);
    step_thread_exit(leader, 7);

    assert!(proc_cap.is_zombie());
    match sysv_sem::execution::step_semctl_with_post(
        semid,
        0,
        sysv_sem::execution::GETVAL,
        sysv_sem::execution::SemCtlArg::None,
        &cred,
        None,
        direct_process_sem_ref_post,
    )
    .expect("GETVAL after exit")
    {
        sysv_sem::execution::SemCtlResult::Val(value) => assert_eq!(value, 2),
        other => panic!("expected Val, got {other:?}"),
    }
}

#[test]
fn exit_group_applies_sysv_sem_undo_adjustments() {
    let _g = setup();
    let proc_cap = bootstrap();
    let ns = proc_cap.nsproxy_cap().expect("nsproxy cap");
    let cred = sign_cred(Cred::root()).expect("root cred cap");
    let semid = sysv_sem::execution::step_semget(
        sysv_shm::execution::IPC_PRIVATE,
        1,
        sysv_shm::execution::IPC_CREAT | 0o600,
        &cred,
        &ns,
    )
    .expect("semget private");
    sysv_sem::execution::step_semctl_with_post(
        semid,
        0,
        sysv_sem::execution::SETVAL,
        sysv_sem::execution::SemCtlArg::Val(2),
        &cred,
        None,
        direct_process_sem_ref_post,
    )
    .expect("SETVAL");
    sysv_sem::execution::step_semop_with_post(
        semid,
        &[sysv_sem::structure::SemBuf {
            sem_num: 0,
            sem_op: -2,
            sem_flg: sysv_sem::structure::sem_flg::SEM_UNDO,
        }],
        &cred,
        &proc_cap,
        direct_process_sem_ref_post,
    )
    .expect("semop SEM_UNDO");

    finish_process_group_for_test(&proc_cap, ExitStatus::Exited(42));

    assert!(proc_cap.is_zombie());
    match sysv_sem::execution::step_semctl_with_post(
        semid,
        0,
        sysv_sem::execution::GETVAL,
        sysv_sem::execution::SemCtlArg::None,
        &cred,
        None,
        direct_process_sem_ref_post,
    )
    .expect("GETVAL after exit_group")
    {
        sysv_sem::execution::SemCtlResult::Val(value) => assert_eq!(value, 2),
        other => panic!("expected Val, got {other:?}"),
    }
}

#[test]
fn fork_child_exit_does_not_apply_parent_sysv_sem_undo_adjustments() {
    let _g = setup();
    let parent = bootstrap();
    let ns = parent.nsproxy_cap().expect("nsproxy cap");
    let cred = sign_cred(Cred::root()).expect("root cred cap");
    let semid = sysv_sem::execution::step_semget(
        sysv_shm::execution::IPC_PRIVATE,
        1,
        sysv_shm::execution::IPC_CREAT | 0o600,
        &cred,
        &ns,
    )
    .expect("semget private");
    sysv_sem::execution::step_semctl_with_post(
        semid,
        0,
        sysv_sem::execution::SETVAL,
        sysv_sem::execution::SemCtlArg::Val(2),
        &cred,
        None,
        direct_process_sem_ref_post,
    )
    .expect("SETVAL");
    sysv_sem::execution::step_semop_with_post(
        semid,
        &[sysv_sem::structure::SemBuf {
            sem_num: 0,
            sem_op: -2,
            sem_flg: sysv_sem::structure::sem_flg::SEM_UNDO,
        }],
        &cred,
        &parent,
        direct_process_sem_ref_post,
    )
    .expect("parent semop SEM_UNDO");

    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");
    finish_process_group_for_test(&child, ExitStatus::Exited(0));

    match sysv_sem::execution::step_semctl_with_post(
        semid,
        0,
        sysv_sem::execution::GETVAL,
        sysv_sem::execution::SemCtlArg::None,
        &cred,
        None,
        direct_process_sem_ref_post,
    )
    .expect("GETVAL after child exit")
    {
        sysv_sem::execution::SemCtlResult::Val(value) => assert_eq!(value, 0),
        other => panic!("expected Val, got {other:?}"),
    }

    finish_process_group_for_test(&parent, ExitStatus::Exited(0));
    match sysv_sem::execution::step_semctl_with_post(
        semid,
        0,
        sysv_sem::execution::GETVAL,
        sysv_sem::execution::SemCtlArg::None,
        &cred,
        None,
        direct_process_sem_ref_post,
    )
    .expect("GETVAL after parent exit")
    {
        sysv_sem::execution::SemCtlResult::Val(value) => assert_eq!(value, 2),
        other => panic!("expected Val, got {other:?}"),
    }
}

#[test]
fn setpgid_to_target_pid_creates_new_group_in_same_session() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");

    let original_session = child.pgrp_cap().session_cap();
    let original_session_id = original_session.sid;

    step_setpgid(&child, Pgid(child.pid.0)).expect("setpgid");

    let new_pgrp = child.pgrp_cap();
    assert_eq!(new_pgrp.pgid, Pgid(child.pid.0));
    assert_eq!(new_pgrp.session_cap().sid, original_session_id);
    assert_ne!(new_pgrp.pgid, parent.pgrp_cap().pgid);
}

#[test]
fn setpgid_with_existing_group_id_is_unimplemented() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");

    // child.pgid != child.pid, and we don't yet support joining an
    // existing group by id (would require session-walk).
    let result = step_setpgid(&child, parent.pgrp_cap().pgid);
    assert!(matches!(result, Err(SetpgidError::Unimplemented)));
}

#[test]
fn setsid_creates_fresh_session_and_pgrp_at_target_pid() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");
    let parent_session_id = parent.pgrp_cap().session_cap().sid;

    let new_sid = step_setsid(&child).expect("setsid");

    assert_eq!(new_sid.0, child.pid.0);
    let pgrp = child.pgrp_cap();
    assert_eq!(pgrp.pgid.0, child.pid.0);
    let session = pgrp.session_cap();
    assert_eq!(session.sid, new_sid);
    assert_ne!(session.sid, parent_session_id);
    assert!(!session.has_controlling_tty());
}

#[test]
fn setsid_rejects_existing_process_group_leader() {
    let _g = setup();
    let parent = bootstrap();

    let result = step_setsid(&parent);

    assert!(
        matches!(result, Err(crate::process::SetsidError::ProcessGroupLeader)),
        "a process-group leader cannot create a new session"
    );
}

#[test]
fn pid_pgid_sid_share_value_space_but_are_distinct_types() {
    let _g = setup();
    let init = bootstrap();

    // After bootstrap: pid=1, pgid=1, sid=1 — same numeric values, but
    // the types prevent accidental swapping in code.
    assert_eq!(init.pid, Pid::INIT);
    assert_eq!(init.pgrp_cap().pgid, Pgid(1));
    assert_eq!(init.pgrp_cap().session_cap().sid.0, 1);
}

#[test]
fn pgrp_member_weak_observation_returns_live_process_until_identity_drops() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");
    let pgrp = parent.pgrp_cap();

    // The pgrp has two members: parent and child.
    assert_eq!(pgrp.member_slot_count(), 2);

    let guard = ebr_guard();
    let live: usize = pgrp
        .members
        .inner
        .lock()
        .iter()
        .filter(|w| w.upgrade(&guard).is_some())
        .count();
    drop(guard);
    assert_eq!(live, 2);

    // To drop the child identity we must release every strong
    // retainer: the test's `child` Cap *and* the parent.children
    // retainer (per §8.5 the parent's children list pins zombies
    // until reap). Reap path:
    //   1. zombify via group exit
    //   2. drop the test's `child` Cap
    //   3. waitpid reap → parent.children removes its Cap
    //   4. epoch drain → identity reclaims
    // After step 4, pgrp.members's Weak is stale.
    let child_pid = child.pid;
    finish_process_group_for_test(&child, ExitStatus::Exited(0));
    drop(child);
    let _ = step_waitpid_nohang(&parent, WaitTarget::Pid(child_pid)).expect("reap");
    tx_test_support::drain_to_quiescence();

    let guard = ebr_guard();
    let live_after: usize = pgrp
        .members
        .inner
        .lock()
        .iter()
        .filter(|w| w.upgrade(&guard).is_some())
        .count();
    drop(guard);
    assert_eq!(
        live_after, 1,
        "after reap + drain, only parent remains live"
    );
}

#[test]
fn fatal_group_exit_records_signum_and_status_encoding() {
    let _g = setup();
    let proc_cap = bootstrap();

    finish_process_group_with_signal_for_test(&proc_cap, Signum::SIGTERM);

    assert!(proc_cap.is_zombie());
    assert_eq!(
        proc_cap.exit_status(),
        Some(ExitStatus::Signaled(Signum::SIGTERM))
    );
    assert_eq!(proc_cap.terminating_signal(), Some(Signum::SIGTERM));
    // POSIX <sys/wait.h> signaled-exit encoding: signum in low 7 bits.
    // (Migrated from the day-1 shell-convention `128 + sig` shape by
    // Wave 1 of the fork/clone/wait4 slice — Open Q #3 DECIDED.)
    assert_eq!(
        proc_cap.exit_status().unwrap().wait_status_word(),
        Signum::SIGTERM.raw() as i32 & 0x7f
    );
    assert_eq!(proc_cap.live_thread_count(), 0);
}

#[test]
fn fatal_group_exit_overrides_terminating_signal_on_double_call() {
    let _g = setup();
    let proc_cap = bootstrap();

    // First call sets terminating_signal=SIGTERM and zombifies.
    finish_process_group_with_signal_for_test(&proc_cap, Signum::SIGTERM);
    assert_eq!(proc_cap.terminating_signal(), Some(Signum::SIGTERM));

    // Second call on the same identity should be a no-op for the
    // payload (already None) but still updates the recorded signal —
    // demonstrates idempotent slot semantics. Defensive coverage of
    // the double-zombify path.
    finish_process_group_with_signal_for_test(&proc_cap, Signum::SIGKILL);
    assert_eq!(proc_cap.terminating_signal(), Some(Signum::SIGKILL));
}

// ----- step_waitpid_nohang (PROCESS_v1 §7.4) -----

#[test]
fn waitpid_with_no_children_returns_no_children() {
    let _g = setup();
    let parent = bootstrap();

    let result = step_waitpid_nohang(&parent, WaitTarget::Any);
    assert_eq!(result, Err(WaitError::NoChildren));
}

#[test]
fn waitpid_with_live_child_returns_none_ready() {
    let _g = setup();
    let parent = bootstrap();
    let _child = step_fork::<TestPmap>(&parent, false, false).expect("fork");

    let result = step_waitpid_nohang(&parent, WaitTarget::Any);
    assert_eq!(
        result,
        Err(WaitError::NoneReady),
        "child exists but is alive — WNOHANG yields NoneReady, not NoChildren"
    );
}

#[test]
fn waitpid_any_reaps_zombie_child_and_returns_status() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");
    let child_pid = child.pid;
    drop(child);
    let payload_was_dropped = |proc_cap: &Cap<ProcessIdentity>| proc_cap.payload.lock().is_none();

    // Get the child back via parent.children() so we can exit it.
    let live = parent.children();
    assert_eq!(live.len(), 1);
    let child = live.into_iter().next().unwrap();
    finish_process_group_for_test(&child, ExitStatus::Exited(42));
    assert!(payload_was_dropped(&child));

    let result = step_waitpid_nohang(&parent, WaitTarget::Any);
    assert_eq!(result, Ok((child_pid, ExitStatus::Exited(42))));
}

#[test]
fn waitpid_specific_pid_reaps_only_that_child() {
    let _g = setup();
    let parent = bootstrap();
    let c1 = step_fork::<TestPmap>(&parent, false, false).expect("c1");
    let c2 = step_fork::<TestPmap>(&parent, false, false).expect("c2");

    finish_process_group_for_test(&c1, ExitStatus::Exited(1));
    finish_process_group_for_test(&c2, ExitStatus::Exited(2));

    // Reap c2 specifically.
    let result = step_waitpid_nohang(&parent, WaitTarget::Pid(c2.pid));
    assert_eq!(result, Ok((c2.pid, ExitStatus::Exited(2))));

    // c1 must still be reapable.
    let result = step_waitpid_nohang(&parent, WaitTarget::Any);
    assert_eq!(result, Ok((c1.pid, ExitStatus::Exited(1))));
}

#[test]
fn waitpid_specific_pid_with_no_match_returns_no_children() {
    let _g = setup();
    let parent = bootstrap();
    let _child = step_fork::<TestPmap>(&parent, false, false).expect("fork");

    // Selector targeting a pid we never forked.
    let result = step_waitpid_nohang(&parent, WaitTarget::Pid(Pid(9999)));
    assert_eq!(result, Err(WaitError::NoChildren));
}

#[test]
fn waitpid_specific_pid_with_live_match_returns_none_ready() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");

    // Match the live child by pid — selector matches but child isn't
    // a zombie yet, so WNOHANG gives NoneReady.
    let result = step_waitpid_nohang(&parent, WaitTarget::Pid(child.pid));
    assert_eq!(result, Err(WaitError::NoneReady));
}

#[test]
fn waitpid_reap_withdraws_from_parent_children_list() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");
    let child_pid = child.pid;

    assert_eq!(parent.child_count(), 1);

    finish_process_group_for_test(&child, ExitStatus::Exited(0));
    drop(child);
    let _ = step_waitpid_nohang(&parent, WaitTarget::Pid(child_pid)).expect("reap succeeds");

    // Parent's children list no longer carries the reaped child's
    // Weak. Slot count drops to 0.
    assert_eq!(parent.child_count(), 0);
    assert_eq!(parent.children().len(), 0);
}

#[test]
fn waitpid_reap_withdraws_from_pgrp_members_list() {
    let _g = setup();
    let parent = bootstrap();
    let pgrp = parent.pgrp_cap();
    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");
    assert_eq!(pgrp.member_slot_count(), 2); // parent + child

    finish_process_group_for_test(&child, ExitStatus::Exited(0));

    // Per PROCESS_v1 §8.5: zombies stay in pgrp.members until reap.
    // The Weak still upgrades because child Cap is still held by the
    // test. (member_slot_count is the raw slot count incl. stale.)
    assert_eq!(pgrp.member_slot_count(), 2);

    let child_pid = child.pid;
    drop(child);
    let _ = step_waitpid_nohang(&parent, WaitTarget::Pid(child_pid)).expect("reap");

    // Reap withdraws from pgrp.members.
    assert_eq!(pgrp.member_slot_count(), 1, "only parent remains in pgrp");
}

#[test]
fn zombie_process_pid_remains_resolvable_until_reap() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");
    let child_pid = child.pid;

    finish_process_group_for_test(&child, ExitStatus::Exited(0));

    assert!(
        crate::process::process_by_pid(child_pid).is_some(),
        "zombie child must remain pid-addressable before parent reap"
    );

    drop(child);
    let _ = step_waitpid_nohang(&parent, WaitTarget::Pid(child_pid)).expect("reap");

    assert!(
        crate::process::process_by_pid(child_pid).is_none(),
        "reap is the pid namespace withdrawal point"
    );
}

#[test]
fn bootstrap_and_topology_steps_register_role_capable_names() {
    let _g = setup();
    let parent = bootstrap();

    match resolve_pid_number_as(parent.pid.0 as u64, PidNameKind::Process) {
        Some(PidName::Process(cap)) => assert_eq!(cap.pid, parent.pid),
        other => panic!("pid should resolve to process name, got {other:?}"),
    }
    let leader = first_thread(&parent);
    match resolve_pid_number_as(leader.tid.0 as u64, PidNameKind::Thread) {
        Some(PidName::Thread(thread)) => assert_eq!(thread.tid, leader.tid),
        other => panic!("leader tid should resolve to thread name, got {other:?}"),
    }
    match resolve_pid_number_as(parent.pgrp_cap().pgid.0 as u64, PidNameKind::ProcessGroup) {
        Some(PidName::ProcessGroup(pgrp)) => assert_eq!(pgrp.pgid, parent.pgrp_cap().pgid),
        other => panic!("pgid should resolve to process-group name, got {other:?}"),
    }
    match resolve_pid_number_as(
        parent.pgrp_cap().session_cap().sid.0 as u64,
        PidNameKind::Session,
    ) {
        Some(PidName::Session(session)) => {
            assert_eq!(session.sid, parent.pgrp_cap().session_cap().sid)
        }
        other => panic!("sid should resolve to session name, got {other:?}"),
    }

    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");
    step_setpgid(&child, Pgid(child.pid.0)).expect("setpgid");
    match resolve_pid_number_as(child.pgrp_cap().pgid.0 as u64, PidNameKind::ProcessGroup) {
        Some(PidName::ProcessGroup(pgrp)) => assert_eq!(pgrp.pgid, child.pgrp_cap().pgid),
        other => panic!("new pgid should resolve to process-group name, got {other:?}"),
    }

    let session_child = step_fork::<TestPmap>(&parent, false, false).expect("fork");
    let sid = step_setsid(&session_child).expect("setsid");
    match resolve_pid_number_as(sid.0 as u64, PidNameKind::Session) {
        Some(PidName::Session(session)) => assert_eq!(session.sid, sid),
        other => panic!("new sid should resolve to session name, got {other:?}"),
    }
}

#[test]
fn waitpid_reap_returns_signaled_status() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");
    let child_pid = child.pid;

    // Child exits via signal — recorded as ExitStatus::Signaled.
    finish_process_group_with_signal_for_test(&child, crate::signal::Signum::SIGTERM);
    drop(child);

    let result = step_waitpid_nohang(&parent, WaitTarget::Pid(child_pid));
    assert_eq!(
        result,
        Ok((
            child_pid,
            ExitStatus::Signaled(crate::signal::Signum::SIGTERM)
        ))
    );
}

#[test]
fn waitpid_after_reaping_all_children_returns_no_children() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");
    finish_process_group_for_test(&child, ExitStatus::Exited(0));
    let child_pid = child.pid;
    drop(child);

    let _ = step_waitpid_nohang(&parent, WaitTarget::Any).expect("first reap");

    // Children list now empty; second wait returns NoChildren.
    let result = step_waitpid_nohang(&parent, WaitTarget::Any);
    assert_eq!(result, Err(WaitError::NoChildren));
    let _ = child_pid; // silence unused-var (kept for narrative)
}

// ----- VFS cwd integration (chdir + getcwd) -----

/// Mint a fresh `RNode` with a small anon page container backing.
/// The RNode's content doesn't matter for cwd-render tests; we just
/// need *some* `Cap<RNode>` to attach to a `DEntry`.
fn fresh_rnode(fs_object_id: u64) -> Cap<RNode> {
    use crate::page_backed::{AnonSwapPolicy, PageContainer, PageContainerKind};
    let pc = PageContainer::new_cap(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Persistent,
        },
        1,
    )
    .expect("page container");
    RNode::new_cap(
        FsObjectId::new(fs_object_id),
        InodeMeta::new(InodeKind::Regular, 0o100644),
        RNodeBacking::PageBacked { pc },
    )
    .expect("rnode")
}

/// Build a root `DEntry` (parent=None, name=ROOT). Synthetic — no
/// real filesystem behind it.
fn fresh_root_dentry() -> Cap<DEntry> {
    DEntry::new_cap(InlineName::ROOT, fresh_rnode(1)).expect("root dentry")
}

/// Build a child `DEntry` named `name`, with `parent` as its parent
/// hint. Mutates the new dentry to install the parent_hint via the
/// payload's `with_*` mutator pattern — but DEntry doesn't expose
/// such a mutator publicly post-allocation. We instead construct an
/// unallocated DEntry with `set_parent_hint`, then sign into the zone.
fn fresh_dentry_under(parent: &Cap<DEntry>, name: &[u8], fs_id: u64) -> Cap<DEntry> {
    let inline = InlineName::new(name).expect("name");
    let mut raw = DEntry::new(inline, fresh_rnode(fs_id));
    raw.set_parent_hint(parent);
    sign::<DEntry>(raw).expect("dentry slot")
}

#[test]
fn getcwd_on_process_with_no_cwd_returns_none() {
    let _g = setup();
    let init = bootstrap();
    // bootstrap_init_process leaves cwd unset on day-1.
    assert_eq!(step_getcwd(&init), None);
}

#[test]
fn chdir_then_getcwd_renders_root_path() {
    let _g = setup();
    let init = bootstrap();
    let root = fresh_root_dentry();

    let outcome = step_chdir(&init, root.clone());
    assert!(matches!(outcome, ChdirOutcome::Replaced { prev: None }));

    let path = step_getcwd(&init).expect("path renders");
    assert_eq!(path.as_slice(), b"/");
}

#[test]
fn chdir_then_getcwd_renders_nested_path() {
    let _g = setup();
    let init = bootstrap();
    let root = fresh_root_dentry();
    let usr = fresh_dentry_under(&root, b"usr", 100);
    let bin = fresh_dentry_under(&usr, b"bin", 101);

    step_chdir(&init, bin);
    let path = step_getcwd(&init).expect("nested path");
    assert_eq!(path.as_slice(), b"/usr/bin");
}

#[test]
fn chdir_pins_cwd_parent_chain_after_external_refs_drop() {
    let _g = setup();
    let init = bootstrap();
    let root = fresh_root_dentry();
    let usr = fresh_dentry_under(&root, b"usr", 100);
    let bin = fresh_dentry_under(&usr, b"bin", 101);

    step_chdir(&init, bin);
    drop(usr);
    drop(root);
    tx_test_support::drain_to_quiescence();

    let path = step_getcwd(&init).expect("nested path after parent refs drop");
    assert_eq!(path.as_slice(), b"/usr/bin");
}

#[test]
fn chdir_returns_previous_cwd_in_replaced() {
    let _g = setup();
    let init = bootstrap();
    let root = fresh_root_dentry();
    let usr = fresh_dentry_under(&root, b"usr", 200);

    step_chdir(&init, root);
    let outcome = step_chdir(&init, usr);
    match outcome {
        ChdirOutcome::Replaced { prev: Some(prev) } => {
            assert!(prev.name().is_empty(), "previous cwd was the root marker");
        }
        other => panic!("expected Replaced{{Some}}, got {other:?}"),
    }
}

#[test]
fn chdir_on_zombie_returns_zombie_ignored() {
    let _g = setup();
    let init = bootstrap();
    let root = fresh_root_dentry();

    finish_process_group_for_test(&init, ExitStatus::Exited(0));
    assert!(init.is_zombie());

    let outcome = step_chdir(&init, root);
    assert!(matches!(outcome, ChdirOutcome::ZombieIgnored));
}

#[test]
fn fork_inherits_parent_cwd() {
    let _g = setup();
    let parent = bootstrap();
    let root = fresh_root_dentry();
    let usr = fresh_dentry_under(&root, b"usr", 300);
    step_chdir(&parent, usr.clone());

    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");

    // Child's cwd renders to the same path as parent's.
    let parent_path = step_getcwd(&parent).expect("parent path");
    let child_path = step_getcwd(&child).expect("child path");
    assert_eq!(parent_path, child_path);
    assert_eq!(child_path.as_slice(), b"/usr");
}

#[test]
fn parent_chdir_after_fork_does_not_affect_child() {
    let _g = setup();
    let parent = bootstrap();
    let root = fresh_root_dentry();
    let usr = fresh_dentry_under(&root, b"usr", 400);
    let var = fresh_dentry_under(&root, b"var", 401);

    step_chdir(&parent, usr);
    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");

    // Parent moves to /var; child's cwd should still be /usr (it
    // got its own Cap<DEntry> at fork time pointing at /usr).
    step_chdir(&parent, var);

    assert_eq!(step_getcwd(&parent).unwrap().as_slice(), b"/var");
    assert_eq!(step_getcwd(&child).unwrap().as_slice(), b"/usr");
}

// ----- waitpid pgrp selectors (PROCESS_v1 §7.4) -----

#[test]
fn waitpid_caller_pgrp_reaps_zombie_in_callers_pgroup() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");
    // Child inherits parent's pgrp at fork. Both share parent.pgrp.

    finish_process_group_for_test(&child, ExitStatus::Exited(5));
    let child_pid = child.pid;
    drop(child);

    let result = step_waitpid_nohang(&parent, WaitTarget::CallerPgrp);
    assert_eq!(result, Ok((child_pid, ExitStatus::Exited(5))));
}

#[test]
fn waitpid_caller_pgrp_skips_child_in_other_pgroup() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");

    // Move child out of parent's pgrp into its own.
    step_setpgid(&child, Pgid(child.pid.0)).expect("setpgid");
    assert_ne!(child.pgrp_cap().pgid, parent.pgrp_cap().pgid);

    finish_process_group_for_test(&child, ExitStatus::Exited(0));

    // CallerPgrp matches only children in caller's pgrp; child is no
    // longer there, so NoChildren — no matching children at all.
    let result = step_waitpid_nohang(&parent, WaitTarget::CallerPgrp);
    assert_eq!(result, Err(WaitError::NoChildren));
}

#[test]
fn waitpid_pgrp_selector_matches_specific_pgid() {
    let _g = setup();
    let parent = bootstrap();
    let c1 = step_fork::<TestPmap>(&parent, false, false).expect("c1");
    let c2 = step_fork::<TestPmap>(&parent, false, false).expect("c2");

    // Move c2 to its own pgrp.
    step_setpgid(&c2, Pgid(c2.pid.0)).expect("setpgid");
    let c2_pgid = c2.pgrp_cap().pgid;

    finish_process_group_for_test(&c1, ExitStatus::Exited(1));
    finish_process_group_for_test(&c2, ExitStatus::Exited(2));
    let c2_pid = c2.pid;
    drop(c1);
    drop(c2);

    // Pgrp(c2_pgid) matches only c2.
    let result = step_waitpid_nohang(&parent, WaitTarget::Pgrp(c2_pgid));
    assert_eq!(result, Ok((c2_pid, ExitStatus::Exited(2))));

    // c1 is still reapable via Any (different pgrp).
    let result = step_waitpid_nohang(&parent, WaitTarget::Any);
    assert!(result.is_ok());
}

#[test]
fn waitpid_pgrp_selector_with_no_matching_pgid_returns_no_children() {
    let _g = setup();
    let parent = bootstrap();
    let _child = step_fork::<TestPmap>(&parent, false, false).expect("fork");

    // Pgid that no child belongs to.
    let result = step_waitpid_nohang(&parent, WaitTarget::Pgrp(Pgid(9999)));
    assert_eq!(result, Err(WaitError::NoChildren));
}

#[test]
fn waitpid_pgrp_selector_with_live_match_returns_none_ready() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");
    let child_pgid = child.pgrp_cap().pgid;

    // Live child in target pgrp — selector matches but no zombie.
    let result = step_waitpid_nohang(&parent, WaitTarget::Pgrp(child_pgid));
    assert_eq!(result, Err(WaitError::NoneReady));
}

// ----- SIGCHLD producer (PROCESS_v1 §7.3.3 phase 5) -----

fn leader_has_sigchld_pending(proc_cap: &Cap<ProcessIdentity>) -> bool {
    let payload = proc_cap.payload.lock();
    let leader = payload.as_ref().expect("alive").threads.nth(0).unwrap();
    drop(payload);
    let leader_payload = leader.payload.lock();
    leader_payload
        .as_ref()
        .expect("alive")
        .pending()
        .is_pending(Signum::SIGCHLD)
}

#[test]
fn child_group_exit_posts_sigchld_to_parent() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");

    assert!(!leader_has_sigchld_pending(&parent));

    finish_process_group_for_test(&child, ExitStatus::Exited(0));

    assert!(
        leader_has_sigchld_pending(&parent),
        "parent's leader thread should have SIGCHLD pending after child zombifies"
    );
}

#[test]
fn child_exit_via_last_thread_cascade_posts_sigchld_to_parent() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");
    let child_leader = first_thread(&child);

    step_thread_exit(child_leader, 7);
    assert!(child.is_zombie());

    assert!(
        leader_has_sigchld_pending(&parent),
        "parent should see SIGCHLD via last-thread cascade too"
    );
}

#[test]
fn bootstrap_init_exit_does_not_panic_with_no_parent() {
    // Init has no parent. SIGCHLD post must short-circuit cleanly.
    let _g = setup();
    let init = bootstrap();
    finish_process_group_for_test(&init, ExitStatus::Exited(0));
    // Just exercising the code path; assertion is "didn't panic".
    assert!(init.is_zombie());
}

#[test]
fn orphaned_child_exit_does_not_post_sigchld() {
    // Parent exits first (severs child); child later exits. With no
    // parent slot to upgrade, the SIGCHLD producer skips silently.
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");

    finish_process_group_for_test(&parent, ExitStatus::Exited(0));
    assert!(child.parent_cap().is_none(), "severed by parent's exit");

    // Now child exits. No parent to receive SIGCHLD; should not
    // panic or error.
    finish_process_group_for_test(&child, ExitStatus::Exited(0));
    assert!(child.is_zombie());
}

#[test]
fn zombie_parent_does_not_receive_sigchld() {
    // If the parent has somehow zombified before the child does
    // (without severing — pathological in spec terms but a defensive
    // shape worth covering), the SIGCHLD producer's underlying
    // process-directed kill returns NoLiveThread and we discard.
    //
    // Note: real flows always sever children at parent exit, so this
    // test simulates the pathological window by manually clearing
    // children before exiting parent (preventing sever from running
    // on the child) — the child keeps a stale parent Weak that
    // upgrades to a zombie identity.
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");

    // Manually clear parent.children so sever doesn't run on the child.
    parent.children.clear();
    finish_process_group_for_test(&parent, ExitStatus::Exited(0));
    assert!(parent.is_zombie());
    // child still has parent slot pointing at the (now zombie) parent.
    assert!(child.parent_cap().is_some());

    // Child exit invokes post_sigchld_to_parent, whose process-directed
    // kill sees a zombie target: NoLiveThread, discarded.
    // No panic.
    finish_process_group_for_test(&child, ExitStatus::Exited(0));
    assert!(child.is_zombie());
}

// ----- INIT_PROCESS slot + reparent-to-init (PROCESS_v1 §8.1) -----

#[test]
fn init_process_handle_returns_none_before_bootstrap() {
    let _g = setup();
    // setup() resets INIT_PROCESS; nothing has bootstrapped yet.
    assert!(init_process().is_none());
}

#[test]
fn init_process_handle_returns_some_after_bootstrap() {
    let _g = setup();
    let init = bootstrap();

    let handle = init_process().expect("init_process registered");
    assert_eq!(handle.pid, init.pid);
    // Same identity by Cap key.
    assert_eq!(handle.key(), init.key());
}

#[test]
fn bootstrap_init_process_errors_if_already_bootstrapped() {
    let _g = setup();
    let _first = bootstrap();

    // Second call without reset.
    let result = bootstrap_init_process(fresh_aspace());
    assert!(matches!(result, Err(BootstrapError::AlreadyBootstrapped)));
}

#[test]
fn non_init_parent_exit_reparents_children_to_init() {
    // init → middle → leaf chain. middle exits. Per §8.1, leaf
    // should reparent to init.
    let _g = setup();
    let init = bootstrap();
    let middle = step_fork::<TestPmap>(&init, false, false).expect("fork middle");
    let leaf = step_fork::<TestPmap>(&middle, false, false).expect("fork leaf");

    assert_eq!(leaf.parent_pid(), middle.pid);
    let init_children_before = init.child_count();

    finish_process_group_for_test(&middle, ExitStatus::Exited(0));

    // leaf's parent now points at init.
    assert_eq!(leaf.parent_pid(), init.pid);
    assert_eq!(leaf.parent_cap().expect("init retained").key(), init.key());
    // init's children list grew by one (received leaf).
    assert_eq!(init.child_count(), init_children_before + 1);
    // middle's children list is now empty.
    assert_eq!(middle.child_count(), 0);
}

#[test]
fn adopted_live_child_auto_reaps_when_it_exits() {
    let _g = setup();
    let init = bootstrap();
    let middle = step_fork::<TestPmap>(&init, false, false).expect("fork middle");
    let leaf = step_fork::<TestPmap>(&middle, false, false).expect("fork leaf");
    let leaf_pid = leaf.pid;
    let init_children_before = init.child_count();

    finish_process_group_for_test(&middle, ExitStatus::Exited(0));
    assert_eq!(leaf.parent_pid(), init.pid);
    assert_eq!(init.child_count(), init_children_before + 1);

    finish_process_group_for_test(&leaf, ExitStatus::Exited(0));

    assert_eq!(
        init.child_count(),
        init_children_before,
        "adopted leaf is auto-reaped; direct child middle remains waitable"
    );
    assert!(
        resolve_pid_number_as(leaf_pid.0 as u64, PidNameKind::Process).is_none(),
        "auto-reap must withdraw adopted orphan from the pid namespace"
    );
}

#[test]
fn adopted_zombie_child_reaped_during_reparent_to_init() {
    let _g = setup();
    let init = bootstrap();
    let middle = step_fork::<TestPmap>(&init, false, false).expect("fork middle");
    let leaf = step_fork::<TestPmap>(&middle, false, false).expect("fork leaf");
    let leaf_pid = leaf.pid;
    let init_children_before = init.child_count();

    finish_process_group_for_test(&leaf, ExitStatus::Exited(0));
    assert!(leaf.is_zombie());

    finish_process_group_for_test(&middle, ExitStatus::Exited(0));

    assert_eq!(
        init.child_count(),
        init_children_before,
        "already-zombie adopted leaf is reaped immediately; middle stays waitable"
    );
    assert!(
        resolve_pid_number_as(leaf_pid.0 as u64, PidNameKind::Process).is_none(),
        "reparent-time reap must withdraw zombie orphan from pid namespace"
    );
}

#[test]
fn init_exit_severs_children_without_reparent_target() {
    // When init itself exits, sever_children's "init is the exiting
    // process" branch fires — children get parent=None rather than
    // being reparented to themselves.
    let _g = setup();
    let init = bootstrap();
    let child = step_fork::<TestPmap>(&init, false, false).expect("fork");

    finish_process_group_for_test(&init, ExitStatus::Exited(0));

    assert_eq!(child.parent_pid(), Pid::RESERVED);
    assert!(child.parent_cap().is_none());
}

// ----- children container (PROCESS_v1 §2.1 + §8.1) -----

#[test]
fn bootstrap_init_has_no_children() {
    let _g = setup();
    let init = bootstrap();

    assert_eq!(init.child_count(), 0);
    assert!(init.children().is_empty());
}

#[test]
fn fork_pushes_child_into_parent_children_list() {
    let _g = setup();
    let parent = bootstrap();
    assert_eq!(parent.child_count(), 0);

    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");

    assert_eq!(parent.child_count(), 1);
    let live = parent.children();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].pid, child.pid);
}

#[test]
fn multiple_forks_accumulate_in_parent_children_list() {
    let _g = setup();
    let parent = bootstrap();

    let c1 = step_fork::<TestPmap>(&parent, false, false).expect("fork 1");
    let c2 = step_fork::<TestPmap>(&parent, false, false).expect("fork 2");
    let c3 = step_fork::<TestPmap>(&parent, false, false).expect("fork 3");

    assert_eq!(parent.child_count(), 3);
    let live_pids: alloc::collections::BTreeSet<_> =
        parent.children().iter().map(|c| c.pid).collect();
    assert_eq!(live_pids.len(), 3);
    assert!(live_pids.contains(&c1.pid));
    assert!(live_pids.contains(&c2.pid));
    assert!(live_pids.contains(&c3.pid));
}

#[test]
fn dropping_test_child_cap_leaves_parent_children_list_intact() {
    // Per §8.5 + §2.1: parent.children retains children with strong
    // Caps until reap. Dropping the test's external Cap on a child
    // does NOT reclaim the child — parent.children still holds a
    // strong ref. Children leave the list only via waitpid reap or
    // when the parent itself reclaims.
    let _g = setup();
    let parent = bootstrap();
    let c1 = step_fork::<TestPmap>(&parent, false, false).expect("fork 1");
    let _c2 = step_fork::<TestPmap>(&parent, false, false).expect("fork 2");

    assert_eq!(parent.child_count(), 2);

    // Drop one external Cap. Parent's children list retains both.
    drop(c1);
    tx_test_support::drain_to_quiescence();

    assert_eq!(
        parent.child_count(),
        2,
        "parent retains child via Cap; drop of external retainer is not enough"
    );
    assert_eq!(parent.children().len(), 2);
}

#[test]
fn parent_group_exit_severs_children_parent_slot() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");
    assert_eq!(child.parent_pid(), parent.pid);

    finish_process_group_for_test(&parent, ExitStatus::Exited(0));

    // Per PROCESS_v1 §8.1 (day-1 stub): child's parent slot is
    // severed. Future reparent-to-init lands when init is globally
    // addressable.
    assert_eq!(child.parent_pid(), Pid::RESERVED);
    assert!(child.parent_cap().is_none());
}

#[test]
fn parent_exit_via_last_thread_cascade_severs_children_parent_slot() {
    // Last-thread cascade exits via step_thread_exit →
    // step_process_exit, not via group exit. Verify both paths
    // sever children.
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");
    let parent_leader = first_thread(&parent);

    step_thread_exit(parent_leader, 7);

    assert!(parent.is_zombie());
    assert_eq!(child.parent_pid(), Pid::RESERVED);
    assert!(child.parent_cap().is_none());
}

#[test]
fn child_severance_does_not_affect_grandchildren() {
    // Sever is shallow: parent's exit only severs its direct
    // children. Grandchildren keep their parent (the now-orphaned
    // child).
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");
    let grandchild = step_fork::<TestPmap>(&child, false, false).expect("fork-of-child");

    assert_eq!(grandchild.parent_pid(), child.pid);

    finish_process_group_for_test(&parent, ExitStatus::Exited(0));

    // child's parent severed (None).
    assert!(child.parent_cap().is_none());
    // grandchild's parent unchanged — still child.
    assert_eq!(grandchild.parent_pid(), child.pid);
}

#[test]
fn normal_group_exit_does_not_set_terminating_signal() {
    let _g = setup();
    let proc_cap = bootstrap();

    finish_process_group_for_test(&proc_cap, ExitStatus::Exited(7));

    assert!(proc_cap.is_zombie());
    assert_eq!(proc_cap.exit_status(), Some(ExitStatus::Exited(7)));
    assert_eq!(proc_cap.terminating_signal(), None);
}

#[test]
fn exit_group_reservation_makes_concurrent_exec_retry_before_detach() {
    let _g = setup();
    let process = bootstrap();
    let leader = first_thread(&process);
    let (reserved, worker_pause) = bounded_test_pause();

    std::thread::scope(|scope| {
        let worker_process = process.clone();
        let exit = scope.spawn(move || {
            crate::process::execution::step_exit_group_with_posts_after_reserve_for_test(
                &worker_process,
                ExitStatus::Exited(17),
                direct_process_task_post,
                direct_process_sem_ref_post,
                move || worker_pause.pause(),
            )
        });

        reserved.wait_until_paused();
        assert_eq!(
            crate::process::ProcessExecPrep::begin(&process, &leader).err(),
            Some(crate::process::ExecPrepError::Again),
        );
        assert!(!process.is_zombie(), "exit is paused before payload detach");
        reserved.release();
        assert_eq!(
            exit.join().expect("exit worker"),
            crate::process::ProcessExitOutcome::Completed,
        );
    });

    assert!(process.is_zombie());
}

#[test]
fn process_payload_aspace_atomic_replace_returns_previous_cap() {
    // Per `txdoc:EXEC-11-PHASE-6-ADDRESS-SPACE-VISIBILITY-BOUNDARY`,
    // exec swaps `ProcessPayload.aspace` atomically and the previous
    // `Cap<AddressSpace>` is returned for EBR-deferred drop. Verify
    // the slot semantics: after `replace_aspace(new)`, `aspace_cap`
    // returns the new `Cap`, and the previous `Cap` is the value we
    // started with.
    let _g = setup();
    let proc_cap = bootstrap();
    let initial = proc_cap.aspace_cap().expect("alive aspace");
    let initial_key = initial.key();

    let replacement = fresh_aspace();
    let replacement_key = replacement.key();

    let prev = proc_cap
        .replace_aspace(replacement)
        .expect("replace returns previous");

    assert_eq!(prev.key(), initial_key, "replace returns the original");
    let post = proc_cap.aspace_cap().expect("still alive");
    assert_eq!(
        post.key(),
        replacement_key,
        "live aspace is the replacement"
    );
    assert_ne!(post.key(), initial_key);
}

mod fd_table;

mod seed_child_leader_context;

mod step_clone_thread;

mod exit_source;

mod posix_wait_status_word;
