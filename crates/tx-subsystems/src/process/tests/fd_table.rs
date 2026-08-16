use super::*;
use crate::process::execution::CloseOp;

// ----- Wave 2 ELF loader plan: per-fd CLOEXEC bitmap + exec phase-7 -----
//
// Process-side tests for the Wave 2 deliverables:
// - `ProcessPayload.fd_cloexec` storage (default 0; set/clear round trip).
// - `step_fork` clones parent's CLOEXEC bits (Linux semantics).
// - `ProcessExecPrep` prepares CLOEXEC closure before PoNR and does not
//   re-enter the fallible close-plan allocation path during commit.
// - `InstallBrkForExecOp` (P3) overwrites both `brk_base` and
//   `current_brk`.

/// Helper: synthesise an `OpenFile` `Cap` over a regular-file RNode so
/// fd-table tests can install slots without standing up a TTY/devfs.
fn fresh_open_file() -> Cap<crate::vfs::OpenFile> {
    use crate::vfs::OpenFileFlags;
    let rnode = fresh_rnode(7777);
    crate::vfs::OpenFile::new_cap(
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
    .expect("open file cap")
}

fn pipe_payload_of(file: &Cap<crate::vfs::OpenFile>) -> Cap<crate::pipe::PipePayload> {
    file.pipe_endpoint()
        .expect("open file must be a pipe endpoint")
        .0
}

fn assert_pipe_read_eof(payload: &Cap<crate::pipe::PipePayload>) {
    let mut buf = [0u8; 4];
    let guard = ebr_guard();
    let outcome =
        crate::pipe::step_read_with_post(payload, &mut buf, &guard, false, |mailbox, event| {
            mailbox.post(event)
        });
    drop(guard);
    assert_eq!(outcome, StepOutcome::Done(0));
}

fn assert_pipe_read_blocks(payload: &Cap<crate::pipe::PipePayload>) {
    let mut buf = [0u8; 4];
    let guard = ebr_guard();
    let outcome =
        crate::pipe::step_read_with_post(payload, &mut buf, &guard, false, |mailbox, event| {
            mailbox.post(event)
        });
    drop(guard);
    assert!(matches!(outcome, StepOutcome::Yield { .. }));
}

#[test]
fn process_payload_fd_cloexec_default_zero() {
    let _g = setup();
    let proc_cap = bootstrap();
    // Every fd defaults to "not CLOEXEC" — bootstrap_init_process
    // initialises the CLOEXEC set empty per the Wave 2 plan (init's
    // stdio is not close-on-exec by Linux convention).
    //
    // fd-ops Wave 1: assert via the snapshot accessor that the set
    // really is empty; the previous AtomicU32 word check is gone.
    assert!(
        proc_cap.fd_cloexec_snapshot().is_empty(),
        "bootstrap CLOEXEC set must start empty"
    );
    for fd in 0u32..16 {
        assert!(!proc_cap.fd_cloexec(fd), "fd {fd} should default to false");
    }
    // fd-ops Wave 1: any `u32` is a valid fd key (the BTreeSet has no
    // upper bound). Pre-Wave-1 the AtomicU32 capped at fd 31; the
    // sparse set lifts that.
    assert!(!proc_cap.fd_cloexec(31));
    assert!(!proc_cap.fd_cloexec(32));
    assert!(!proc_cap.fd_cloexec(64));
}

#[test]
fn process_payload_set_fd_cloexec_round_trip() {
    let _g = setup();
    let proc_cap = bootstrap();

    proc_cap.set_fd_cloexec(3, true);
    assert!(proc_cap.fd_cloexec(3));
    assert!(!proc_cap.fd_cloexec(2));
    assert!(!proc_cap.fd_cloexec(4));

    // Toggling another bit must not perturb the first.
    proc_cap.set_fd_cloexec(5, true);
    assert!(proc_cap.fd_cloexec(3));
    assert!(proc_cap.fd_cloexec(5));

    // Clearing fd 3 leaves fd 5 alone.
    proc_cap.set_fd_cloexec(3, false);
    assert!(!proc_cap.fd_cloexec(3));
    assert!(proc_cap.fd_cloexec(5));

    // fd-ops Wave 1: large fd values (> 31) are now legal — the
    // sparse `BTreeSet<u32>` has no upper bound. Pre-Wave-1 the
    // AtomicU32 silently dropped these.
    proc_cap.set_fd_cloexec(100, true);
    assert!(proc_cap.fd_cloexec(100));
    assert!(proc_cap.fd_cloexec(5));
    proc_cap.set_fd_cloexec(100, false);
    assert!(!proc_cap.fd_cloexec(100));
}

// ---------------------------------------------------------------------------
// fd-ops Wave 1 (2026-05-07): sparse `BTreeMap`/`BTreeSet` fd table.
// ---------------------------------------------------------------------------

/// fd-ops Wave 1: any `u32` fd is a valid key in the sparse
/// `BTreeMap<u32, Cap<OpenFile>>` table. Pre-Wave-1 the table was a
/// fixed `[Option<Cap<OpenFile>>; 8]` array and the install at fd 100
/// would have been silently dropped.
#[test]
fn process_payload_fds_btreemap_supports_sparse_fd_above_31() {
    let _g = setup();
    let proc_cap = bootstrap();

    let file = fresh_open_file();
    let prev = proc_cap.set_fd(100, Some(file));
    assert!(prev.is_none(), "fd 100 was not previously occupied");

    assert!(proc_cap.fd(100).is_some(), "fd 100 must be observable");
    // No other fds occupy.
    for fd in [0u32, 1, 2, 3, 7, 31, 32, 99, 101, 200] {
        assert!(
            proc_cap.fd(fd).is_none(),
            "fd {fd} must be empty (only fd 100 was set)"
        );
    }

    let removed = proc_cap.set_fd(100, None);
    assert!(removed.is_some(), "removing fd 100 returns the prior file");
    assert!(
        proc_cap.fd(100).is_none(),
        "fd 100 must be empty post-remove"
    );
}

#[test]
fn close_op_returns_the_atomically_removed_file() {
    let _g = setup();
    let proc_cap = bootstrap();
    let old_file = fresh_open_file();
    let old_object = old_file.rnode().fs_object_id();
    proc_cap.set_fd(10, Some(old_file));
    proc_cap.set_fd_cloexec(10, true);

    let mut op = CloseOp {
        process: proc_cap.clone(),
        fd: 10,
    };
    let mut ctx = ScriptCtx::<ProcessIdentity>::new();
    let closed = match op.step(&mut ctx) {
        StepOutcome::Done(file) => file,
        other => panic!("close expected Done(file), got {other:?}"),
    };

    assert_eq!(closed.rnode().fs_object_id(), old_object);
    assert!(proc_cap.fd(10).is_none(), "close must remove the old fd");
    assert!(!proc_cap.fd_cloexec(10), "close must clear FD_CLOEXEC");

    let mut repeated = CloseOp {
        process: proc_cap.clone(),
        fd: 10,
    };
    assert!(matches!(
        repeated.step(&mut ctx),
        StepOutcome::Err(crate::process::adapter::step_engine::Errno::EBADF)
    ));

    let replacement = fresh_open_file();
    let replacement_object = replacement.rnode().fs_object_id();
    proc_cap.set_fd(10, Some(replacement));
    assert_eq!(
        proc_cap
            .fd(10)
            .expect("replacement remains installed")
            .rnode()
            .fs_object_id(),
        replacement_object
    );
}

#[test]
fn close_range_detaches_current_fds_and_clears_stale_cloexec() {
    let _g = setup();
    let proc_cap = bootstrap();
    for fd in [3, 4, 9, 100] {
        proc_cap.set_fd(fd, Some(fresh_open_file()));
    }
    proc_cap.set_fd_cloexec(4, true);
    proc_cap.set_fd_cloexec(9, true);
    proc_cap.set_fd_cloexec(500, true);

    let closed = proc_cap.take_fds_for_close_range(4, 500);

    assert_eq!(closed.len(), 3);
    assert!(proc_cap.fd(3).is_some(), "fd below the range survives");
    for fd in [4, 9, 100] {
        assert!(proc_cap.fd(fd).is_none(), "fd {fd} is detached");
        assert!(!proc_cap.fd_cloexec(fd), "fd {fd} cloexec is cleared");
    }
    assert!(!proc_cap.fd_cloexec(500), "stale cloexec is cleared");
}

/// fd-ops Wave 1: `allocate_fd` returns the lowest unused fd ≥ 0.
/// Walks the BTreeMap's sorted keys looking for the first gap.
#[test]
fn process_payload_allocate_fd_returns_lowest_unused() {
    let _g = setup();
    let proc_cap = bootstrap();

    // Empty table: lowest unused fd is 0.
    assert_eq!(proc_cap.allocate_fd(), 0);

    // Install fd 0 and fd 2 — leaving fd 1 as the gap.
    proc_cap.set_fd(0, Some(fresh_open_file()));
    proc_cap.set_fd(2, Some(fresh_open_file()));
    assert_eq!(proc_cap.allocate_fd(), 1, "fd 1 is the lowest gap");

    // Plug the gap; lowest unused fd shifts to 3.
    proc_cap.set_fd(1, Some(fresh_open_file()));
    assert_eq!(proc_cap.allocate_fd(), 3);

    // `next_fd_above` is the same scan with a non-zero floor.
    assert_eq!(proc_cap.next_fd_above(2), 3);
    assert_eq!(proc_cap.next_fd_above(10), 10);
}

#[test]
fn process_payload_install_fd_pair_uses_two_lowest_slots_and_cloexec() {
    let _g = setup();
    let proc_cap = bootstrap();
    proc_cap.set_fd(0, Some(fresh_open_file()));
    proc_cap.set_fd(2, Some(fresh_open_file()));
    let payload = proc_cap.payload_cap().expect("live process payload");

    let installed = payload
        .install_new_fd_pair(fresh_open_file(), fresh_open_file(), true)
        .expect("two descriptors fit");

    assert_eq!(installed, (1, 3));
    assert!(proc_cap.fd(1).is_some());
    assert!(proc_cap.fd(3).is_some());
    assert!(proc_cap.fd_cloexec(1));
    assert!(proc_cap.fd_cloexec(3));
}

#[test]
fn process_payload_install_fd_pair_has_no_partial_commit_at_limit() {
    let _g = setup();
    let proc_cap = bootstrap();
    proc_cap.set_rlimit_nofile(4, 4);
    proc_cap.set_fd(0, Some(fresh_open_file()));
    proc_cap.set_fd(1, Some(fresh_open_file()));
    proc_cap.set_fd(2, Some(fresh_open_file()));
    let payload = proc_cap.payload_cap().expect("live process payload");

    assert!(
        payload
            .install_new_fd_pair(fresh_open_file(), fresh_open_file(), true)
            .is_none(),
        "one remaining slot cannot publish half a pair"
    );
    assert!(
        proc_cap.fd(3).is_none(),
        "failed pair leaves fd table unchanged"
    );
    assert!(!proc_cap.fd_cloexec(3));
}

#[test]
fn process_payload_fd_pair_rollback_preserves_reused_descriptor() {
    let _g = setup();
    let proc_cap = bootstrap();
    let payload = proc_cap.payload_cap().expect("live process payload");
    let first = fresh_open_file();
    let second = fresh_open_file();
    let (first_fd, second_fd) = payload
        .install_new_fd_pair(first.clone(), second.clone(), true)
        .expect("descriptor pair");

    let replacement = fresh_open_file();
    let _ = payload.install_fd_with_cloexec(first_fd, replacement.clone(), false);
    let removed = payload.take_fd_pair_if_matches(first_fd, &first, second_fd, &second);

    assert_eq!(
        removed.len(),
        1,
        "only the unchanged endpoint is rolled back"
    );
    assert_eq!(payload.fd(first_fd), Some(replacement));
    assert!(payload.fd(second_fd).is_none());
    assert!(!payload.fd_cloexec_get(first_fd));
    assert!(!payload.fd_cloexec_get(second_fd));
}

/// fd-ops Wave 1: `install_fd` returns the previous occupant so the
/// caller can EBR-defer-drop the displaced `Cap<OpenFile>`. Matches
/// the `dup2`/`dup3` shape (Wave 4).
#[test]
fn process_payload_install_fd_returns_previous_occupant() {
    let _g = setup();
    let proc_cap = bootstrap();

    let first = fresh_open_file();
    let second = fresh_open_file();

    // First install: slot was empty.
    let prev1 = proc_cap.install_fd(5, first);
    assert!(
        prev1.is_none(),
        "first install at fd 5 has no prior occupant"
    );

    // Second install at the same fd: the first occupant returns.
    let prev2 = proc_cap.install_fd(5, second);
    assert!(
        prev2.is_some(),
        "second install at fd 5 must return the first occupant"
    );

    assert!(proc_cap.fd(5).is_some(), "fd 5 must remain installed");
}

/// fd-ops Wave 1: `step_fork`'s fd-table clone walks the parent's
/// `BTreeMap` entries (sparse fds included), not a 0..8 array index
/// loop. The child sees every parent fd, including fds > 31.
#[test]
fn process_payload_step_fork_clones_sparse_fd_table() {
    let _g = setup();
    let parent = bootstrap();

    // Parent's fd table: fds 0, 1, 2 (the canonical stdio shape) plus
    // fd 100 (the sparse case the BTreeMap migration unlocks).
    parent.set_fd(0, Some(fresh_open_file()));
    parent.set_fd(1, Some(fresh_open_file()));
    parent.set_fd(2, Some(fresh_open_file()));
    parent.set_fd(100, Some(fresh_open_file()));

    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");

    // Child inherits the entire sparse map.
    assert!(child.fd(0).is_some(), "child inherits fd 0");
    assert!(child.fd(1).is_some(), "child inherits fd 1");
    assert!(child.fd(2).is_some(), "child inherits fd 2");
    assert!(child.fd(100).is_some(), "child inherits sparse fd 100");
    assert!(child.fd(3).is_none(), "fd 3 was never set; child sees None");

    // Mutating the child must not bleed back into the parent.
    child.set_fd(100, None);
    assert!(child.fd(100).is_none());
    assert!(
        parent.fd(100).is_some(),
        "parent's fd 100 survives the child's close"
    );
}

#[test]
fn process_payload_close_pipe_writer_fd_publishes_eof_immediately() {
    let _g = setup();
    let proc_cap = bootstrap();
    let (reader, writer) =
        crate::pipe::step_pipe2(crate::pipe::PipeFlags::default()).expect("pipe2");
    let payload = pipe_payload_of(&reader);

    proc_cap.set_fd(3, Some(reader));
    proc_cap.set_fd(4, Some(writer));

    proc_cap.set_fd(4, None);

    assert_pipe_read_eof(&payload);
    proc_cap.set_fd(3, None);
}

#[test]
fn process_payload_dup_pipe_writer_keeps_pipe_alive_until_all_writer_fds_close() {
    let _g = setup();
    let proc_cap = bootstrap();
    let (reader, writer) =
        crate::pipe::step_pipe2(crate::pipe::PipeFlags::default()).expect("pipe2");
    let payload = pipe_payload_of(&reader);

    proc_cap.set_fd(3, Some(reader));
    proc_cap.set_fd(4, Some(writer));

    let mut op = DupOp {
        process: proc_cap.clone(),
        oldfd: 4,
    };
    let mut ctx = ScriptCtx::<ProcessIdentity>::new();
    let dupfd = match op.step(&mut ctx) {
        StepOutcome::Done(fd) => fd,
        other => panic!("expected dup Done(fd), got {other:?}"),
    };

    proc_cap.set_fd(4, None);
    assert_pipe_read_blocks(&payload);

    proc_cap.set_fd(dupfd, None);
    assert_pipe_read_eof(&payload);
    proc_cap.set_fd(3, None);
}

#[test]
fn step_fork_accounts_inherited_pipe_writer_fd() {
    let _g = setup();
    let parent = bootstrap();
    let (reader, writer) =
        crate::pipe::step_pipe2(crate::pipe::PipeFlags::default()).expect("pipe2");
    let payload = pipe_payload_of(&reader);

    parent.set_fd(3, Some(reader));
    parent.set_fd(4, Some(writer));
    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");

    parent.set_fd(4, None);
    assert_pipe_read_blocks(&payload);

    child.set_fd(4, None);
    assert_pipe_read_eof(&payload);
    parent.set_fd(3, None);
    child.set_fd(3, None);
}

#[test]
fn child_exit_drains_inherited_pipe_writer_fd_and_publishes_eof() {
    let _g = setup();
    let parent = bootstrap();
    let (reader, writer) =
        crate::pipe::step_pipe2(crate::pipe::PipeFlags::default()).expect("pipe2");
    let payload = pipe_payload_of(&reader);

    parent.set_fd(3, Some(reader));
    parent.set_fd(4, Some(writer));
    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");

    parent.set_fd(4, None);
    assert_pipe_read_blocks(&payload);

    // Model the real cross-hart reader: install a mailbox subscription before
    // the child's last writer disappears. The exit fd-drain must use the
    // caller-provided owner-aware route rather than PipePayload's direct
    // no-reactor fallback.
    let mailbox = Arc::new(TaskMailbox::new());
    let generation = mailbox.next_generation();
    let _registration = payload
        .reader_endpoint()
        .prepare(
            Arc::downgrade(&mailbox),
            generation,
            tx_substrate::step::InterestMask::new(crate::pipe::PIPE_READABLE),
        )
        .install_if(|| true)
        .expect("pipe reader subscription");
    let mut owner_aware_posts = 0usize;
    step_exit_group_with_posts(
        &child,
        ExitStatus::Exited(0),
        direct_process_task_post,
        |mailbox, event| {
            owner_aware_posts += 1;
            mailbox.post_with_scheduler_hint(
                event,
                tx_substrate::wake::MailboxSchedulerHint::LifecycleWake,
            )
        },
    );

    assert_eq!(
        owner_aware_posts, 1,
        "exit-time EOF uses injected wake route"
    );
    assert!(matches!(
        mailbox.poll(),
        Some(MailboxEvent::SourceFired { source, .. })
            if source.raw() == payload.reader_source_id()
    ));
    assert_pipe_read_eof(&payload);
    parent.set_fd(3, None);
}

/// fd-ops Wave 1: the CLOEXEC `BTreeSet<u32>` accepts arbitrary `u32`
/// keys — pre-Wave-1 the `AtomicU32` silently dropped fds ≥ 32.
#[test]
fn process_payload_fd_cloexec_btreeset_supports_sparse_fds_above_31() {
    let _g = setup();
    let proc_cap = bootstrap();

    proc_cap.set_fd_cloexec(100, true);
    assert!(proc_cap.fd_cloexec(100));
    assert!(!proc_cap.fd_cloexec(99));
    assert!(!proc_cap.fd_cloexec(101));

    // Snapshot reflects only the high fd.
    let snap = proc_cap.fd_cloexec_snapshot();
    assert_eq!(snap.len(), 1);
    assert!(snap.contains(&100));

    proc_cap.set_fd_cloexec(100, false);
    assert!(!proc_cap.fd_cloexec(100));
    assert!(proc_cap.fd_cloexec_snapshot().is_empty());
}

#[test]
fn step_fork_clones_fd_cloexec_bits() {
    let _g = setup();
    let parent = bootstrap();
    parent.set_fd_cloexec(1, true);
    parent.set_fd_cloexec(4, true);

    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");

    // Child inherits parent's snapshot at fork time.
    assert!(child.fd_cloexec(1));
    assert!(child.fd_cloexec(4));
    assert!(!child.fd_cloexec(0));
    assert!(!child.fd_cloexec(2));

    // Mutating the child must not bleed back into the parent.
    child.set_fd_cloexec(2, true);
    assert!(child.fd_cloexec(2));
    assert!(!parent.fd_cloexec(2));

    // Mutating the parent post-fork must not bleed into the child.
    parent.set_fd_cloexec(0, true);
    assert!(parent.fd_cloexec(0));
    assert!(!child.fd_cloexec(0));
}

#[test]
fn prepared_cloexec_commit_closes_only_the_prevalidated_file() {
    let _g = setup();
    let process = bootstrap();
    let leader = first_thread(&process);
    let closing = fresh_open_file();
    let kept = fresh_open_file();
    process.set_fd(3, Some(closing));
    process.set_fd(4, Some(kept));
    process.set_fd_cloexec(3, true);

    let old_aspace = process.aspace_cap().expect("old aspace");
    let replacement = fresh_aspace();
    let replacement_key = replacement.key();
    let prep =
        crate::process::ProcessExecPrep::begin(&process, &leader).expect("reserve exec lifecycle");
    let close_plan = prep
        .prepare_cloexec_close()
        .expect("prepare CLOEXEC close plan");
    let previous = prep
        .replace_aspace_and_close_cloexec(replacement, close_plan)
        .expect("commit address space and CLOEXEC close plan");

    assert_eq!(previous.key(), old_aspace.key());
    assert_eq!(
        process.aspace_cap().expect("new aspace").key(),
        replacement_key
    );
    assert!(process.fd(3).is_none());
    assert!(process.fd(4).is_some());
    assert!(!process.fd_cloexec(3));
}

#[test]
fn stale_cloexec_plan_rolls_back_before_aspace_swap_and_preserves_reused_fd() {
    let _g = setup();
    let process = bootstrap();
    let leader = first_thread(&process);
    let original = fresh_open_file();
    process.set_fd(3, Some(original));
    process.set_fd_cloexec(3, true);
    let old_aspace_key = process.aspace_cap().expect("old aspace").key();

    let prep =
        crate::process::ProcessExecPrep::begin(&process, &leader).expect("reserve exec lifecycle");
    let close_plan = prep
        .prepare_cloexec_close()
        .expect("prepare CLOEXEC close plan");
    let replacement_file = fresh_open_file();
    let replacement_file_key = replacement_file.key();
    let _old = process.set_fd(3, Some(replacement_file));
    process.set_fd_cloexec(3, true);

    assert_eq!(
        prep.replace_aspace_and_close_cloexec(fresh_aspace(), close_plan)
            .err(),
        Some(crate::process::ExecPrepError::Again),
    );
    assert_eq!(
        process.aspace_cap().expect("unchanged aspace").key(),
        old_aspace_key
    );
    assert_eq!(
        process.fd(3).expect("reused fd survives").key(),
        replacement_file_key
    );
    assert!(process.fd_cloexec(3));
}

#[test]
fn post_prepare_cloexec_plan_allocation_fault_is_not_consumed_by_commit() {
    let _g = setup();
    let process = bootstrap();
    let leader = first_thread(&process);
    process.set_fd(3, Some(fresh_open_file()));
    process.set_fd_cloexec(3, true);

    let prep =
        crate::process::ProcessExecPrep::begin(&process, &leader).expect("reserve exec lifecycle");
    let close_plan = prep
        .prepare_cloexec_close()
        .expect("prepare CLOEXEC close plan");
    crate::process::exec_prep::fail_next_cloexec_plan_allocation_for_test();

    prep.replace_aspace_and_close_cloexec(fresh_aspace(), close_plan)
        .expect("commit must not allocate another CLOEXEC plan");
    assert!(crate::process::exec_prep::cloexec_plan_allocation_fault_pending_for_test());
    crate::process::exec_prep::clear_cloexec_plan_allocation_fault_for_test();
}

#[test]
fn install_brk_for_exec_resets_both_brk_base_and_current() {
    use crate::process::exec_prep::install_brk_for_exec;
    use crate::process::execution::BOOTSTRAP_BRK_BASE;
    let _g = setup();
    let proc_cap = bootstrap();

    // Bootstrap state: both brk_base and current_brk seeded to the
    // same bootstrap value (per the existing
    // `bootstrap_init_process` contract).
    assert_eq!(proc_cap.brk_base(), BOOTSTRAP_BRK_BASE);
    assert_eq!(proc_cap.current_brk(), BOOTSTRAP_BRK_BASE);

    // Simulate a userspace brk(2) advance so current_brk diverges
    // from brk_base — this is the "running process" state exec
    // takes over.
    proc_cap.set_current_brk(BOOTSTRAP_BRK_BASE + 0x1000);
    assert_eq!(proc_cap.current_brk(), BOOTSTRAP_BRK_BASE + 0x1000);
    assert_eq!(proc_cap.brk_base(), BOOTSTRAP_BRK_BASE);

    // Install fresh exec-image brk: both fields rewritten to the
    // same new value (per `txdoc:EXEC-12-4-INSTALL-BRK`).
    let new_brk: u64 = 0xb000_0000;
    install_brk_for_exec(&proc_cap, new_brk);

    assert_eq!(proc_cap.brk_base(), new_brk);
    assert_eq!(proc_cap.current_brk(), new_brk);
}

// ----- Wave 1 fork/clone/wait4 slice (2026-05-06) -----
//
// Tests for the kernel-side prerequisites Wave 2's `sys_clone` and
// `sys_wait4` syscall arms will consume:
//   - `seed_child_leader_context` (Part 1A): the syscall driver
//     helper that stamps `regs[10] = 0` (RV64 a0) and `pc + 4`
//     onto the child leader thread's saved trap context.
//   - `ProcessPayload.exit_source` (Part 1B): the per-process wait
//     channel that fires on child zombification, so a parent
//     parked on `sys_wait4` wakes when any child exits.
//   - POSIX `wait_status_word` migration (Open Q #3 DECIDED):
//     `(code & 0xff) << 8` for explicit exits and `sig & 0x7f` for
//     signal exits.
