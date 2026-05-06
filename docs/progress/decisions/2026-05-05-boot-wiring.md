# Boot wiring — pid=1 globally addressable + kernel boot path

**Date:** 2026-05-05
**Branch:** `process-topology` (continued from `step_waitpid_nohang`)
**Status:** Complete. CI green (11 gates). 312 tests pass (307 + 5 net).

## Goal

Make `pid=1` init globally addressable by:

1. Stashing the bootstrap `Cap` in a process-subsystem static slot.
2. Having `bootstrap_init_process` register itself in that slot.
3. Wiring the tx-kernel boot path to actually call
   `bootstrap_init_process` after substrate / zone / reactor init.
4. Upgrading `sever_children` to **reparent** children to init when
   the global handle is set (real `PROCESS_v1` §8.1 semantics) —
   instead of the day-1 sever-only stub.

This closes the largest remaining process-subsystem spec gap: the
"reparent to init" arm of §8.1, and gives every other subsystem a
kernel-static init reference for future SIGCHLD-to-init,
session-leader-tty hangup, and orphan-pgrp detection.

## What landed

### Global init slot in `process/execution.rs`

```rust
static INIT_PROCESS: SpinMutex<Option<Cap<ProcessIdentity>>> = SpinMutex::new(None);

pub fn init_process() -> Option<Cap<ProcessIdentity>> {
    INIT_PROCESS.lock().clone()
}

#[cfg(test)]
pub(crate) fn reset_init_process_for_test() {
    *INIT_PROCESS.lock() = None;
}
```

`SpinMutex<Option<Cap<...>>>` is the same slot shape every other
process-subsystem field uses. Migration to a future
`tx_substrate::AtomicSlot<T>` is a subsystem-internal change later.

The slot retains a strong `Cap` so init outlives every other
reference — POSIX init lifetime semantics.

### `BootstrapError` + registration

```rust
pub enum BootstrapError {
    Zone(ZoneError),
    AlreadyBootstrapped,
}

pub fn bootstrap_init_process(aspace: Cap<AddressSpace>)
    -> Result<Cap<ProcessIdentity>, BootstrapError>
{
    if INIT_PROCESS.lock().is_some() {
        return Err(BootstrapError::AlreadyBootstrapped);
    }
    // ... existing construction (sign_session, sign_process_group,
    //     sign_process_identity, sign_thread, sign_process_payload) ...
    *INIT_PROCESS.lock() = Some(proc_cap.clone());
    Ok(proc_cap)
}
```

Return type changed from `Result<.., ZoneError>` to
`Result<.., BootstrapError>`. All callers (tests + new
tx-kernel/init.rs site) use `.expect(...)` — transparent.

### `sever_children` upgraded to three-case logic

```rust
fn sever_children(process: &Cap<ProcessIdentity>) {
    let children: Vec<Cap<ProcessIdentity>> =
        core::mem::take(&mut *process.children.lock());

    let init = init_process();
    let target = init.as_ref().filter(|i| i.key() != process.key());

    if let Some(init) = target {
        let init_weak = init.downgrade();
        let mut init_children = init.children.lock();
        for child in children {
            *child.parent.lock() = Some(init_weak);
            init_children.push(child);
        }
    } else {
        for child in children {
            *child.parent.lock() = None;
        }
    }
}
```

Three cases:

1. **Init handle present AND init ≠ exiting process.** Move each
   child `Cap` from `process.children` into `init.children`; update
   each child's `parent` slot to `Weak<init>`. Real §8.1
   reparent-to-init semantics.
2. **Init handle is the exiting process** (init itself is dying).
   Sever-only — no higher-level reaper exists; children become
   orphans with `parent = None`.
3. **Init handle absent** (test pre-bootstrap; pre-process-init at
   boot). Sever-only — same observable shape as case 2.

`mem::take` drains `process.children` in all cases, so the dying
process owns no child Caps post-sever.

### tx-kernel boot path

[crates/tx-kernel/src/init.rs](../../../crates/tx-kernel/src/init.rs):

```rust
fn init_substrate_if_ready(handoff: BootHandoff) {
    if P::SUBSTRATE_BOOT_READY {
        // ... existing init through run_bsp_reactor_timer_idle_smoke ...
        Self::init_process_subsystem();    // NEW
        // remaining deferred slots: VFS / devices / userspace
    }
}

fn init_process_subsystem() {
    let aspace = tx_subsystems::vm::AddressSpace::new_cap_for_platform::<P>()
        .expect("init aspace");
    let _init = tx_subsystems::process::bootstrap_init_process(aspace)
        .expect("bootstrap init");
    Self::write_board_sentinel_prefix();
    tx_hal::console_write_str::<P>(":process:init:ok\n");
}
```

`P: TxPlatform` already implies `PmapIf` per [tx-hal/src/lib.rs:1346](../../../crates/tx-hal/src/lib.rs#L1346)
supertrait chain — no new bound needed. Local `Cap` drops at end
of scope; `INIT_PROCESS` retains.

Sentinel `txkernel:<board>:process:init:ok` is additive — no
existing patterns change.

### Test isolation

Every test setup() helper now resets the slot:

```rust
fn setup() -> ... {
    // ... existing init, drains, counter resets ...
    reset_init_process_for_test();
    guard
}
```

Wired into 5 test files:
- [process/tests.rs](../../../crates/tx-subsystems/src/process/tests.rs)
- [signal/tests.rs](../../../crates/tx-subsystems/src/signal/tests.rs) (4 setup() sites
  — top-level + 3 inner modules each via `super::*`)
- [cred/tests.rs](../../../crates/tx-subsystems/src/cred/tests.rs)
- [thread_runtime/tests.rs](../../../crates/tx-subsystems/src/thread_runtime/tests.rs)
- [tty/tests/typed_session_pgrp.rs](../../../crates/tx-subsystems/src/tty/tests/typed_session_pgrp.rs)

One existing test required maintenance:
`weak_owner_proc_flips_dead_after_identity_drop` releases the
INIT_PROCESS retainer explicitly via `reset_init_process_for_test()`
mid-test (before drain) — because dropping the test's local `Cap`
is no longer sufficient when `INIT_PROCESS` holds the second strong
retainer.

### New tests (5)

- `init_process_handle_returns_none_before_bootstrap` — slot starts
  empty (per setup() reset).
- `init_process_handle_returns_some_after_bootstrap` — accessor
  returns the registered Cap; same identity by `key()`.
- `bootstrap_init_process_errors_if_already_bootstrapped` —
  `BootstrapError::AlreadyBootstrapped` on second call.
- `non_init_parent_exit_reparents_children_to_init` — fork chain
  init → middle → leaf; exit middle; assert leaf reparents to
  init (parent_pid, parent_cap.key, init.child_count grows,
  middle.child_count goes to 0).
- `init_exit_severs_children_without_reparent_target` — init exits;
  child gets `parent = None` (case 2 of `sever_children`'s three-case
  logic).

### Existing test stability

Every existing parent-exit test uses `parent = bootstrap()` — so
parent **is** init. With reparenting on, those exits hit the "init
is the exiting process" branch ⇒ sever-only ⇒ same observable
behavior as before. All 27 pre-existing process tests stay green
without modification.

## Spec compliance

| Spec | Pre | Post |
|---|---|---|
| §8.1 reparent-to-init | ❌ sever-only stub | ✓ real reparent when init alive |
| §8.1 sever fallback for init exit | ❌ (no init concept) | ✓ case 2 |
| §8.1 substrate `structural_move` primitive | ❌ absent | ❌ deferred (we use direct lock+update) |
| Globally-addressable init handle | ❌ absent | ✓ `process::init_process()` |
| Kernel boot creates pid=1 | ❌ absent | ✓ `init_process_subsystem()` |
| Boot-time substrate / zone / process ordering | partial | ✓ explicit, sentinel-marked |

## Deliberately deferred

- **Init's executing thread.** `bootstrap_init_process` constructs
  init's leader `ThreadIdentity` + `ThreadPayload`, but the thread
  is not submitted to the reactor — pid=1 is a structural anchor
  for now. First-userspace task submission lands with
  exec / userspace-stub.
- **PR_SET_CHILD_SUBREAPER.** Phase 2. Reparenting always goes to
  init; subreaper-ancestor lookup is the v2 refinement.
- **Init replacement / re-exec.** POSIX-correctly, init can never
  exit (kernel panic) and only exec replaces its image. Day-1's
  `BootstrapError::AlreadyBootstrapped` enforces "no second init"
  but doesn't yet wire the panic-on-init-exit invariant; that
  arrives with the §7.3.3 phase-5 SIGCHLD-to-init full cascade.
- **Substrate `structural_move`.** Spec §8.1 prescribes it for
  reparenting. We use direct lock + push. Substrate work.
- **`zones::register_all()` extracted as a dedicated boot step.**
  Currently rides along inside `run_zone_smoke`. Independent
  cleanup; functionally equivalent.
- **First-userspace surface.** Init has no userspace image, no
  ELF load, no scheduler submission. EXEC_v1 + first-userspace
  spec land later.

## Verification

- `cargo xtask ci` — 11/11 gates green.
- `cargo test -p tx-subsystems --lib -- --test-threads=1` — 312
  tests pass (307 prior + 5 new).
- `cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
  — green (boot path compiles).
- `cargo xtask progress validate` — ok.
- `cargo xtask lint docs` — ok.

## Commit ledger

- `<this commit>` — `process: INIT_PROCESS slot + bootstrap registers (PROCESS_v1 §8.1)`
- `<this commit>` — `process: sever_children reparents to init when handle set (PROCESS_v1 §8.1)`
- `<this commit>` — `tx-kernel: init_process_subsystem boot step calls bootstrap_init_process`
- `<this commit>` — `docs(progress): record boot wiring landing`

## Branch summary so far

The `process-topology` branch now carries 16 commits. The day-1
process subsystem closes:

- Topology (Process / Thread / ProcessGroup / Session)
- Signal day-1 (post + observe + Gewalt routing)
- Cred service stub + permission check
- TTY pgrp typed dispatch
- Signal delivery sweep + ast_dispatch
- step_exit_group_with_signal materialising route_sigkill
- Drift cleanup + foreground-pgrp ratification (P3 / TTY-owned)
- Children container + parent binding bidirectional
- SIGCHLD producer in step_process_exit / step_exit_group
- step_waitpid_nohang reaping
- **Boot wiring + reparent-to-init**

The full reap cycle now works end-to-end:
`fork → exit → SIGCHLD-to-parent → waitpid → reap`, plus reparenting
when intermediate parents exit.

## Next step

Of the items in the waitpid note's next-step list, post-boot-wiring:

1. **§8.3 session-leader-tty hangup cascade** — doc-spelled in P3.
   Self-contained: detect "process is session leader" (compare
   process.pid against session.sid via `process.pgrp_cap().session_cap()`),
   then run the four-step cascade in `step_process_exit`. Now
   buildable on top of `Session::foreground_pgrp_cap()`.
2. **`SigInfo` carrier** for SIGCHLD's `si_pid` / `si_uid` /
   `si_code` / `si_status`. Independent.
3. **Pgrp selectors** for waitpid (`Pgrp(Pgid)` / `CallerPgrp`).
   ~30min filter additions on the children walk.
4. **Blocking `waitpid`.** Needs reactor channel integration.

Of these, (1) is the most spec-completing and now unblocked.

## Blockers

None.
