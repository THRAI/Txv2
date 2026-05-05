# TTY pgrp typed-rebinding

**Date:** 2026-05-05
**Branch:** `process-topology` (continued from cred day-1)
**Status:** Complete. CI green (11 gates). 7 new tests pass; total suite 234.

## Goal

Wire `TtyIdentity.session_pgrp` to the real process-subsystem
entities (`Cap<Session>` / `Cap<ProcessGroup>`) so signal-fanout code
can hand the foreground pgrp directly to `signal::step_kill_pgrp`
instead of reaching into a u32 pgrp-id and rediscovering the entity.

## What landed

### `SessionPgrp` shape

```rust
#[derive(Clone, Copy, Debug)]
pub struct SessionPgrp {
    // Raw POSIX ids (legacy fast-path; no epoch guard required).
    pub session_id:           u32,
    pub session_leader_pgid:  u32,
    pub foreground_pgid:      u32,
    // Typed entity links.
    pub session:         Option<Weak<Session>>,
    pub foreground_pgrp: Option<Weak<ProcessGroup>>,
}
```

`PartialEq` is manual: only the id triplet is compared so legacy
tests that build raw-id bindings still match by value.

Constructors:
- `SessionPgrp::from_raw_ids(session_id, leader_pgid, foreground_pgid)` —
  legacy path with `None` typed refs.
- `SessionPgrp::from_typed(&session, &foreground_pgrp)` — caches
  ids out of the caps and downgrades both to `Weak`.

Accessors:
- `upgrade_session() -> Option<Cap<Session>>`
- `upgrade_foreground_pgrp() -> Option<Cap<ProcessGroup>>`

### `TtyIdentity` accessors

- `bind_session_pgrp_typed(&session, &fg_pgrp)` — convenience for
  building a typed binding and installing it.
- `foreground_pgrp_cap() -> Option<Cap<ProcessGroup>>` — upgrade the
  current binding's foreground-pgrp Weak. Returns `None` for legacy
  raw-id bindings or when the pgrp has been dropped.

### Required substrate / vm bumps

Two follow-on changes needed to make the new typed refs flow through
the existing static-SpinMutex chain reaching back from the TTY
registry to `Cap<AddressSpace>`:

1. **`tx-substrate::zone::Weak<T>` Clone/Copy made unconditional.**
   The previous `#[derive(Clone, Copy)]` synthesised `T: Clone` /
   `T: Copy` bounds (the standard derive quirk for types containing
   `PhantomData<T>`), which prevented `Weak<Session>` /
   `Weak<ProcessGroup>` from being Clone since `Session` and
   `ProcessGroup` aren't Clone (they wrap `SpinMutex`). Replaced
   with manual impls:
   ```rust
   impl<T: 'static> Clone for Weak<T> { fn clone(&self) -> Self { *self } }
   impl<T: 'static> Copy for Weak<T> {}
   ```
   `Weak<T>` carries only `(raw: u32, generation: u16, _marker:
   PhantomData<T>)` — there's nothing T-typed to clone.

2. **`unsafe impl Send + Sync for AddressSpace`.** Putting `Weak<Session>`
   into `SessionPgrp` (via `AtomicSlot<SessionPgrp>` on the static
   `HARDWARE_TTYS` SpinMutex) drags the entity graph through the
   Send+Sync chain: `Weak<Session>` → `Session: Send+Sync` → ... →
   `Cap<ProcessGroup>` → ... → `Cap<ProcessIdentity>` →
   `PayloadCap<ProcessPayload>` → `ProcessPayload: Send+Sync` →
   `Cap<AddressSpace>` → `AddressSpace: Send+Sync`.
   `AddressSpace` is !Send by transitive containment of substrate
   `MapPin` tokens (which carry `PhantomData<*const ()>` to enforce
   per-CPU pinning at the page-allocator level). The kernel's
   epoch+pmap discipline already covers cross-CPU access to
   AddressSpace contents, so this is a kernel-level
   shared-by-discipline lift, not a soundness change to MapPin.
   Documented in the unsafe impl block.

### Test suite split

`crates/tx-subsystems/src/tty/tests.rs` was at 1384 lines pre-pass;
adding the new typed-pgrp tests pushed past the 1500-line arch lint
ceiling. Converted from a single file into a directory:

```
tty/tests/
├── mod.rs                     declares submodules
├── legacy_phase_a.rs          existing 1386-line phase-A suite
└── typed_session_pgrp.rs      7 new tests, 168 lines
```

The 7 existing test sites that constructed `SessionPgrp { ... }`
literally inside `legacy_phase_a.rs` were converted to
`SessionPgrp::from_raw_ids(...)` calls — semantic-equivalent change
since `PartialEq` ignores typed refs.

### Tests (7 new)

In `tty/tests/typed_session_pgrp.rs`:

1. `from_raw_ids_leaves_typed_refs_none`
2. `from_typed_caches_ids_and_stores_weak_refs`
3. `tty_bind_session_pgrp_typed_installs_binding_and_exposes_pgrp_cap`
4. `foreground_pgrp_cap_returns_none_for_legacy_raw_id_binding`
5. `typed_pgrp_can_drive_step_kill_pgrp_against_real_membership` —
   the integration round-trip: bind typed → upgrade →
   `signal::step_kill_pgrp` → assert delivery count.
6. `typed_session_survives_setpgid_until_session_drops`
7. `typed_session_flips_dead_after_setsid_drops_old_session`

## Deliberately deferred

- **Migrating `IoctlCaller.pgrp_id: u32` and `SignalTarget::CallerProcessGroup(u32)`**
  to typed refs. Keeps tty/checks/require_fg_pgrp.rs compiling
  against the existing raw-id comparison. Lands when the syscall
  driver passes a real `Cap<ProcessIdentity>` into ioctl call sites.
- **Auto-update `bind_session_pgrp` to call `from_typed` when given
  caps**: the existing `bind_session_pgrp(SessionPgrp)` setter
  preserves whatever the caller passes; callers that have caps in
  hand should use `bind_session_pgrp_typed` explicitly.
- **Real driver of bind_session_pgrp_typed at TIOCSCTTY commit**:
  the ioctl builds raw-id `SessionPgrp` from `IoctlCaller.{session_id,
  pgrp_id}` because the caller cred carries u32 ids today.
  Migrates with the previous bullet.

## Verification

- `cargo xtask ci` — 11/11 gates green.
- `cargo test -p tx-subsystems --lib -- --test-threads=1` — 234
  tests pass (227 prior + 7 new).
- `cargo xtask progress validate` — ok.
- `cargo xtask lint docs` — ok.

## Commit ledger

- `<this commit>` — `tty: typed Cap<Session>/Cap<ProcessGroup> rebinding for SessionPgrp`
- `<this commit>` — `docs(progress): record TTY pgrp typed-rebinding completion`

## Next step

The `process-topology` branch now carries:
- Process / Thread / ProcessGroup / Session topology (commit `3652bd1`)
- Signal day-1: types + post-and-observe shims (commit `c982b81`)
- Cred service stub: types + setuid/setgid + cred-on-payload (commit `51d0b2a`)
- TTY pgrp typed-rebinding (this commit)

Recommended follow-ups:

1. **Permission check in kill** (~30 min). Wraps
   `step_kill_process` / `step_kill_pgrp` with a cred-aware variant
   that checks `source.shares_euid(target) || source.is_privileged_for(KILL)`.
   First real consumer of both `Cred` and `signal::step_kill_*` in a
   permissioned path.
2. **Migrate `IoctlCaller`/`SignalTarget` to typed refs** (~1 session).
   Drops the u32-pgrp-id in TTY's job-control flow and routes the
   pgrp Cap end-to-end so ioctl can directly call `step_kill_pgrp`.
3. **Signal delivery sweep** (~2 sessions). Reactor-side step that
   consults `signal_mask`, `pending`, and `sig_actions`, optionally
   invokes a handler, and applies the default action for uncaught
   signals.
4. **Boot wiring** (~1 session). Thread `bootstrap_init_process`
   through `tx-kernel/src/init.rs`.

## Blockers

None.
