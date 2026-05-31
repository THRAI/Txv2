use super::*;

// ----- Wave 2 ELF loader plan: per-fd CLOEXEC bitmap + exec phase-7 -----
//
// Process-side tests for the Wave 2 deliverables:
// - `ProcessPayload.fd_cloexec` storage (default 0; set/clear round trip).
// - `step_fork` clones parent's CLOEXEC bits (Linux semantics).
// - `step_close_cloexec_fds` (P1) closes only marked fds and clears the
//   bitmap.
// - `step_install_brk_for_exec` (P3) overwrites both `brk_base` and
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
    let outcome = crate::pipe::step_read(payload, &mut buf, &guard, false);
    drop(guard);
    assert_eq!(outcome, StepOutcome::Done(0));
}

fn assert_pipe_read_blocks(payload: &Cap<crate::pipe::PipePayload>) {
    let mut buf = [0u8; 4];
    let guard = ebr_guard();
    let outcome = crate::pipe::step_read(payload, &mut buf, &guard, false);
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

/// Semantic close removes the fd and clears its CLOEXEC bit in one
/// operation. `set_fd(fd, None)` intentionally remains a low-level
/// table edit for replacement-style tests.
#[test]
fn process_payload_close_fd_removes_fd_and_cloexec_bit() {
    let _g = setup();
    let proc_cap = bootstrap();

    proc_cap.set_fd(7, Some(fresh_open_file()));
    proc_cap.set_fd_cloexec(7, true);
    proc_cap.set_fd_cloexec(8, true);

    let closed = proc_cap.close_fd(7);

    assert!(closed.is_some(), "close_fd returns the detached file");
    assert!(proc_cap.fd(7).is_none(), "fd 7 is closed");
    assert!(
        !proc_cap.fd_cloexec(7),
        "successful close clears the matching CLOEXEC bit"
    );
    assert!(
        proc_cap.fd_cloexec(8),
        "unrelated CLOEXEC bits are untouched"
    );
}

#[test]
fn process_payload_close_fd_missing_preserves_cloexec_state() {
    let _g = setup();
    let proc_cap = bootstrap();

    proc_cap.set_fd_cloexec(9, true);

    let closed = proc_cap.close_fd(9);

    assert!(
        closed.is_none(),
        "missing fd close reports no detached file"
    );
    assert!(
        proc_cap.fd_cloexec(9),
        "failed close preserves CLOEXEC state for EBADF semantics"
    );
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

    step_exit_group(&child, ExitStatus::Exited(0));
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
fn step_close_cloexec_fds_closes_marked_fds_clears_others() {
    use crate::process::exec_prep::step_close_cloexec_fds;
    let _g = setup();
    let proc_cap = bootstrap();

    // Install three open files at fds 0, 1, 2; mark only fd 1 as
    // CLOEXEC.
    proc_cap.set_fd(0, Some(fresh_open_file()));
    proc_cap.set_fd(1, Some(fresh_open_file()));
    proc_cap.set_fd(2, Some(fresh_open_file()));
    proc_cap.set_fd_cloexec(1, true);

    step_close_cloexec_fds(&proc_cap);

    // Only fd 1 should be closed; the others remain.
    assert!(proc_cap.fd(0).is_some(), "fd 0 was not marked; survives");
    assert!(
        proc_cap.fd(1).is_none(),
        "fd 1 was marked CLOEXEC; should be closed"
    );
    assert!(proc_cap.fd(2).is_some(), "fd 2 was not marked; survives");
}

#[test]
fn step_close_cloexec_fds_clears_bitmap_after() {
    use crate::process::exec_prep::step_close_cloexec_fds;
    let _g = setup();
    let proc_cap = bootstrap();

    proc_cap.set_fd(2, Some(fresh_open_file()));
    proc_cap.set_fd_cloexec(2, true);
    assert!(proc_cap.fd_cloexec(2));

    step_close_cloexec_fds(&proc_cap);

    // The sweep clears the set wholesale: future fcntl(F_SETFD) calls
    // start from a clean state.
    assert!(
        !proc_cap.fd_cloexec(2),
        "post-sweep, the CLOEXEC bit must be cleared"
    );
    assert!(
        proc_cap.fd_cloexec_snapshot().is_empty(),
        "post-sweep, the CLOEXEC set must be empty"
    );
}

#[test]
fn step_install_brk_for_exec_resets_both_brk_base_and_current() {
    use crate::process::exec_prep::step_install_brk_for_exec;
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
    step_install_brk_for_exec(&proc_cap, new_brk);

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
