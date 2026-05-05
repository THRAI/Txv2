# Foreground pgrp homing — ratify TTY-owned (P3)

**Date:** 2026-05-05
**Branch:** `process-topology` (continued from process drift cleanup)
**Status:** Complete. CI green (11 gates). 285 tests pass (282 + 3 new).

## Goal

Close the spec/spec contradiction the process audit surfaced:
`PROCESS_v1` §2.4 declared `Session.foreground_pgrp` while `TTY.md` and
`OBJECT_PATTERN_FIXES_v1.md` OPA-3 (recommended) put the slot on
`TtyIdentity.session_pgrp`. The impl had already followed OPA-3; the
docs hadn't caught up. Per the architectural analysis (subsystem
encapsulation, hangup atomicity, bundled-pair invariant, lifecycle
alignment), TTY-owned wins. This pass ratifies that decision in code,
docs, and lint.

## What landed

### `Session::foreground_pgrp_cap()` helper

```rust
impl Session {
    pub fn controlling_tty_cap(&self) -> Option<Cap<TtyIdentity>> {
        let weak = (*self.controlling_tty.lock())?;
        let guard = tx_substrate::epoch::guard();
        weak.upgrade(&guard)
    }

    pub fn foreground_pgrp_cap(&self) -> Option<Cap<ProcessGroup>> {
        self.controlling_tty_cap()?.foreground_pgrp_cap()
    }
}
```

Hides the two-hop weak dereference (`Session.controlling_tty` →
`TtyIdentity` → `tty.foreground_pgrp_cap()`). Future
process-side callers (session-leader-death SIGHUP cascade per
PROCESS_v1 §8.3, orphan-pgrp detection per §8.2) read fg pgrp through
this single call instead of open-coding the chain.

3 new tests:
- `session_foreground_pgrp_cap_resolves_two_hop_via_controlling_tty` —
  happy path: session ↔ tty wired both ways, fg pgrp resolves.
- `session_foreground_pgrp_cap_none_without_controlling_tty` —
  first-hop fail.
- `session_foreground_pgrp_cap_none_when_tty_has_no_binding` —
  second-hop fail (controlling_tty present, tty.session_pgrp empty).

### `PROCESS_v1` §2.4 amendment

Removed the stale `foreground_pgrp: AtomicSlot<Option<Binding<ProcessGroup>>>`
field declaration from `Session`. Replaced the description with:

- `controlling_tty` is now described as the **mirror** of the
  authoritative `TtyIdentity.session_pgrp`, not an authoritative
  binding in its own right.
- Added an explicit "Foreground process group — not stored on Session"
  paragraph pointing at OPA-3 (TTY-CTL-1) and describing the two-hop
  dereference pattern with the ASCII diagram.
- Spelled out that either weak upgrade may return `None` and callers
  must handle it.

### `PROCESS_v1` §8.3 amendment

Rewrote the session-leader-death cascade to reflect TTY-owned homing:

1. Resolve fg pgrp via `session.foreground_pgrp_cap()` (None-tolerant).
2. If both hops succeed, deliver SIGHUP (and SIGCONT per POSIX
   §11.1.3).
3. Clear the tty's `session_pgrp` slot — the authoritative side.
4. Clear `session.controlling_tty` — the mirror.

Added an explicit atomicity note: the four steps are class-3
compositional per `BINDING_v1`, and POSIX doesn't constrain
intermediate-state visibility.

### `OBJECT_PATTERN_FIXES_v1.md` OPA-3 amendment

- Replaced "Recommended: TTY-owned ..." hedging with
  **"Decided: TTY-owned ..."**.
- Removed the "There are two valid ownership choices. Pick one and
  make it explicit." preamble.
- Restructured the comparison table to show authoritative vs. derived
  for the *resolved* shape, not the open question.
- Expanded the "Why" with the four-leg architectural argument
  (subsystem encapsulation, hangup atomicity, bundled-pair invariant,
  lifecycle alignment, cross-subsystem-write-once).
- Added new invariant **TTY-CTL-1a**: "No `foreground_pgrp` field on
  Session — the authoritative slot lives on
  `TtyIdentity.session_pgrp`."
- Header changed from "Required invariant:" to "Required invariants
  (enforced by `cargo xtask lint arch`)" — the lint backs the doc.

### `cargo xtask lint arch` rules

Two new line-by-line checks in `xtask/src/lint.rs::lint_arch_text`:

- **TTY-CTL-1**: rejects `Cap<ProcessIdentity>` substring in
  `crates/tx-subsystems/src/tty/structure/identity.rs` (would catch
  the regression OPA-3 originally identified — leader-process caps
  used as session/pgrp substitute).
- **TTY-CTL-1a**: rejects `foreground_pgrp:` field declaration in
  `crates/tx-subsystems/src/process/structure.rs`. Skips comment
  lines (`///`, `//!`, `//`) so doc text mentioning the field on
  TTY's side is fine.

6 new lint unit tests covering: rejection of leader cap; allowance
of `Weak<Session>`/`Weak<ProcessGroup>`; rejection of session-side
fg_pgrp field; allowance of `foreground_pgrp_cap(` method (the
helper above); allowance of doc-comment mentions.

## Spec compliance

| Spec | Pre | Post |
|---|---|---|
| PROCESS_v1 §2.4 declares fg_pgrp on Session | ❌ stale field | ✓ removed; points at TTY |
| PROCESS_v1 §8.3 spells out two-hop dereference | ❌ presumed Session-side | ✓ explicit |
| OPA-3 ratifies TTY-owned (vs. recommends) | ❌ "two valid choices" | ✓ "Decided" |
| TTY-CTL-1 enforced by lint | ❌ doc-only | ✓ `cargo xtask lint arch` |
| TTY-CTL-1a invariant exists | ❌ implied | ✓ explicit |
| Session helper for two-hop dereference | ❌ open-code | ✓ `Session::foreground_pgrp_cap()` |

## Deliberately deferred

- **TTY-CTL-2 lint** (PtyMaster controlling-session forwarding-only)
  not added. Pty subsystem doesn't carry a separate `PtyMaster`
  identity yet; lint can't fire against a non-existent shape. Lands
  with the pty pass.
- **Session-leader-death cascade implementation** still future —
  PROCESS_v1 §8.3 now describes the spec-correct flow but the code
  in `step_process_exit` remains a stub. Blocked on the children
  container (which the previous decision note identified as the next
  natural step).
- **`Session.controlling_tty` rename** to make the mirror nature
  explicit (e.g. `controlling_tty_mirror`) — declined; the field is
  read from process-side code that doesn't need to know about the
  TTY-side authoritative slot, and the impl already documents the
  mirror semantics in PROCESS_v1 §2.4.

## Verification

- `cargo xtask ci` — 11/11 gates green.
- `cargo test -p tx-subsystems --lib -- --test-threads=1` — 285
  tests pass (282 prior + 3 new).
- `cargo test -p xtask --lib` — 43 tests pass (37 prior + 6 new
  TTY-CTL-1/1a).
- `cargo xtask progress validate` — ok.
- `cargo xtask lint docs` — ok.

## Commit ledger

- `<this commit>` — `process: add Session::foreground_pgrp_cap() two-hop helper (OPA-3)`
- `<this commit>` — `docs: ratify TTY-owned foreground pgrp in PROCESS_v1 §2.4 + §8.3`
- `<this commit>` — `docs(meta): OPA-3 changes Recommended → Decided; add TTY-CTL-1a`
- `<this commit>` — `xtask: lint TTY-CTL-1 + TTY-CTL-1a (no Cap<ProcessIdentity> in TTY identity; no foreground_pgrp field on process side)`
- `<this commit>` — `docs(progress): record P3 ratification`

## Next step

The `process-topology` branch's signal/process/topology story is now
fully spec-aligned at the field-name and primitive-mechanism level
for everything not blocked on missing substrate primitives. Per the
process drift cleanup decision note, the natural next item is the
**children container** on `ProcessIdentity` — unblocks
`script_waitpid`, reparenting (PROCESS_v1 §8.1), orphan-pgrp SIGHUP
detection (§8.2), and the SIGCHLD edge of `step_process_exit` (§7.3.3
phase 5). With `parent: SpinMutex<Option<Weak<ProcessIdentity>>>`
already in place from the previous pass, the children side is the
matching downward materialization.

## Blockers

None.
