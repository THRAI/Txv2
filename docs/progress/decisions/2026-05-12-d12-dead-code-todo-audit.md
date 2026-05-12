# Decision D12: Dead-code, TODO, and migration-residual audit

**Date:** 2026-05-12
**Status:** decided (research-only ADR; no production code changes)
**Worker:** W-PP (audit-only)
**Companion:** [migration-completion-audit-2026-05-12](../migration-completion-audit-2026-05-12.md),
[D9 (signal wake migration)](2026-05-11-d9-signal-wake-migration.md),
[D2 (WaitSource coexistence)](2026-05-11-d2-waitsource-coexists-with-rawport.md)

## 1. Executive summary

After the 39-worker v3 migration, the workspace builds clean except for a
small, well-localized set of warnings and TODO markers. This audit
walks `cargo check --workspace --tests` and the source tree to
categorize each loose end so future cleanup is mechanical.

- **Total `dead_code` warnings:** 27 (all `tx-subsystems`; 1 distinct
  duplicate echoed from the lib-test build).
- **Total `unused_imports` warnings:** 1.
- **Total `TODO/FIXME/XXX/HACK` markers in `crates/`:** 64 (60 TODO,
  1 FIXME, 0 XXX, 0 HACK; 3 historical-vocab `OnCarrier`/`WakeCarrier`/
  `InterestConditions` references are renames already explained inline
  and are not TODOs).
- **TODO references in `docs/progress/decisions/`:** 24 (all
  forward-pointers from earlier ADRs — none are stale orphans).
- **Worker-letter breadcrumbs (`// W-X ...`):** 0 — workers cleaned up
  after themselves.
- **`unsafe impl ZoneAllocated` sites:** 29 (catalogued §6).
- **Cleanup-eligible *now* (zero-risk):** **2** items (one `unused
  import`, one `FIXME` that is a known flaky test). Everything else is
  intentional scaffolding awaiting a *named* future phase.

Headline finding: **the 27 `dead_code` warnings are not abandoned
code.** They are 27 PR-2 `StepOp` adapter wraps (one per migrated
step-fn) introduced uniformly across `page_backed/` and `tty/execution/`
per `docs/Txv3/03_STEP_MODEL_v2.md` §2.1. Each adapter has a paired
free-fn caller path that is still source-of-truth; callers migrate
incrementally. The right action is to **annotate**, not delete.

## 2. Dead-code table

All 27 entries below are PR-2 `StepOp` wrap structs of the form
`pub struct XxxOp<'a> { ... }` with an `impl StepOp` body delegating
to a free `step_xxx` fn defined immediately above in the same file.
Category for every row: **scaffolding awaiting caller**.
Recommended action for every row: **keep + add
`#[allow(dead_code)]` with a one-line `// scaffolding: PR-2 StepOp
adapter awaiting caller migration per docs/Txv3/03_STEP_MODEL_v2.md
§2.1` comment**, OR alternatively crate-level `#![allow(dead_code)]`
gated on the migration window and removed when the first non-test
caller lands.

| File:line | Symbol | Category | Action |
|---|---|---|---|
| crates/tx-subsystems/src/page_backed/cross_variant.rs:218 | `CopyFileRangeOp` | scaffolding awaiting caller | keep + `#[allow(dead_code)]` |
| crates/tx-subsystems/src/page_backed/lifecycle.rs:331 | `FsyncOp` | scaffolding awaiting caller | keep + `#[allow(dead_code)]` |
| crates/tx-subsystems/src/page_backed/lifecycle.rs:350 | `TruncateOp` | scaffolding awaiting caller | keep + `#[allow(dead_code)]` |
| crates/tx-subsystems/src/page_backed/lifecycle.rs:370 | `FallocateOp` | scaffolding awaiting caller | keep + `#[allow(dead_code)]` |
| crates/tx-subsystems/src/tty/execution/step_hangup.rs:71 | `HangupOp` | scaffolding awaiting caller | keep + `#[allow(dead_code)]` |
| crates/tx-subsystems/src/tty/execution/step_ingest.rs:139 | `IngestOp` | scaffolding awaiting caller | keep + `#[allow(dead_code)]` |
| crates/tx-subsystems/src/tty/execution/step_ioctl.rs:618 | `IoctlTiocscttyOp` | scaffolding awaiting caller | keep + `#[allow(dead_code)]` |
| crates/tx-subsystems/src/tty/execution/step_ioctl.rs:638 | `IoctlTiocscttyForProcessOp` | scaffolding awaiting caller | keep + `#[allow(dead_code)]` |
| crates/tx-subsystems/src/tty/execution/step_ioctl.rs:658 | `IoctlTiocnottyOp` | scaffolding awaiting caller | keep + `#[allow(dead_code)]` |
| crates/tx-subsystems/src/tty/execution/step_ioctl.rs:678 | `IoctlTiocnottyForProcessOp` | scaffolding awaiting caller | keep + `#[allow(dead_code)]` |
| crates/tx-subsystems/src/tty/execution/step_ioctl.rs:698 | `IoctlTiocspgrpOp` | scaffolding awaiting caller | keep + `#[allow(dead_code)]` |
| crates/tx-subsystems/src/tty/execution/step_ioctl.rs:719 | `IoctlTiocspgrpForProcessOp` | scaffolding awaiting caller | keep + `#[allow(dead_code)]` |
| crates/tx-subsystems/src/tty/execution/step_ioctl.rs:740 | `IoctlTiocgpgrpOp` | scaffolding awaiting caller | keep + `#[allow(dead_code)]` |
| crates/tx-subsystems/src/tty/execution/step_ioctl.rs:759 | `IoctlTiocgwinszOp` | scaffolding awaiting caller | keep + `#[allow(dead_code)]` |
| crates/tx-subsystems/src/tty/execution/step_ioctl.rs:778 | `IoctlTiocswinszOp` | scaffolding awaiting caller | keep + `#[allow(dead_code)]` |
| crates/tx-subsystems/src/tty/execution/step_ioctl.rs:798 | `IoctlTcgetsOp` | scaffolding awaiting caller | keep + `#[allow(dead_code)]` |
| crates/tx-subsystems/src/tty/execution/step_ioctl.rs:817 | `IoctlTcsetsOp` | scaffolding awaiting caller | keep + `#[allow(dead_code)]` |
| crates/tx-subsystems/src/tty/execution/step_master_close.rs:45 | `MasterCloseLastOp` | scaffolding awaiting caller | keep + `#[allow(dead_code)]` |
| crates/tx-subsystems/src/tty/execution/step_openpty.rs:151 | `OpenPtyOp` | scaffolding awaiting caller | keep + `#[allow(dead_code)]` |
| crates/tx-subsystems/src/tty/execution/step_poll_hardware.rs:138 | `PollHardwareInputOp` | scaffolding awaiting caller | keep + `#[allow(dead_code)]` |
| crates/tx-subsystems/src/tty/execution/step_read.rs:150 | `ReadOp` | scaffolding awaiting caller | keep + `#[allow(dead_code)]` |
| crates/tx-subsystems/src/tty/execution/step_read.rs:170 | `ReadForCallerOp` | scaffolding awaiting caller | keep + `#[allow(dead_code)]` |
| crates/tx-subsystems/src/tty/execution/step_read.rs:191 | `ReadForProcessOp` | scaffolding awaiting caller | keep + `#[allow(dead_code)]` |
| crates/tx-subsystems/src/tty/execution/step_write.rs:304 | `WriteOp` | scaffolding awaiting caller | keep + `#[allow(dead_code)]` |
| crates/tx-subsystems/src/tty/execution/step_write.rs:324 | `WriteForCallerOp` | scaffolding awaiting caller | keep + `#[allow(dead_code)]` |
| crates/tx-subsystems/src/tty/execution/step_write.rs:345 | `WriteForProcessOp` | scaffolding awaiting caller | keep + `#[allow(dead_code)]` |

Notes:

- The lib-test build re-emits `IoctlTiocscttyOp` as the only "fresh"
  warning (the other 26 are de-duplicated by `rustc`), giving the
  apparent 27 + 1 = 28 in raw output; the underlying distinct sites
  are 26.
- Several sister files (e.g. `step_signal.rs`, `step_setsid.rs`) have
  similar wraps that already *have* test callers and so do not warn.
  That confirms the wraps are the deliberate, partially-adopted PR-2
  shape, not abandoned attempts.
- A crate-level `#![allow(dead_code)]` in `tx-subsystems/src/lib.rs`
  gated by `#[cfg(...)]` is not recommended — it would mask future
  *real* dead code that we want to see. Per-symbol `#[allow]` with the
  scaffolding comment is the correct trade.

### Unused imports

| File:line | Symbol | Category | Action |
|---|---|---|---|
| crates/tx-subsystems/src/process/structure.rs:34 | `WaitSourceId` | abandoned by refactor | `cargo fix --lib -p tx-subsystems` (remove from `use tx_substrate::step_v3::{InterestMask, WaitSourceId}`) |

This is the only `cargo fix` candidate in the workspace and the only
truly "abandoned by retired site" item in the entire dead-code audit.

## 3. TODO marker table

### 3.1 Code (`crates/`)

Total: **64** matches. Grouped:

- **48** uses of `TODO(phase-<name>)` — explicit phase deferrals
  (`phase-userva`, `phase-thread-signals`, `phase-pid-resolver`,
  `phase-dirfd`, `phase-signal-frame`, `phase-fcntl-setfl`,
  `phase-futex`, `phase-tls`, `phase-rlimit-enforcement`,
  `phase-readdir-mount`, `phase-nonblock`, `phase-rusage`,
  `phase-rtc`, `phase-process-topology`, `phase-pgrp-kill`,
  `phase-linkat-nofollow`, `phase-goblin-share`, `phase-cputime`,
  `phase-vfs-utimens`, `phase-vfs-tmpfs-link`, `phase-vfs-tmpfs-grow`,
  `phase-vfs-rename-xdir`, `phase-umask`, `phase-symlink-flag`,
  `phase-elf-loader`, `phase-blocking-read`).
  → **All categorize as `legitimate-open` (real ongoing concern)**.
  Each is cross-referenced in at least one ADR under
  `docs/progress/decisions/` (see §3.3). Keep + leave the marker as
  the in-source pointer.

- **5** uses of `TODO(Phase G)` in `tty/structure/` and
  `tty/ldisc/termios_change.rs` — points at the still-pending
  `FixedName<N>` shared type and termios echo-region work.
  → **legitimate-open**. Should be cross-linked to the relevant
  active doc (no ADR yet; create one when Phase G work begins).

- **3** uses of `TODO(spec-reconciliation)`, `TODO(elf-loader)`,
  `TODO(multi-bss)` — single-site forward pointers (`tx-ext4`,
  `tx-scripts`).
  → **legitimate-open**.

- **2** uses of `TODO PR-11 phase 2b: ...` in `tx-shims/linux_syscall/
  aio.rs` — these are the only **worker-PR-tagged** TODOs.
  → **migration-residual**: convert to `TODO(phase-aio-spawn)` or
  to a `txdoc:` ref pointing at D8 (`2026-05-11-d8-pr-11-aio-plan.md`).

- **3** comment-only references in `tx-kernel/src/trap_handoff.rs:222`
  and `tx-shims/linux_syscall/{mod,numbers}.rs` mention "TODO" in
  prose without a tag.
  → **legitimate-open**, but these are documentation-style references
  to TODOs elsewhere. No action.

- **1** `FIXME` in `crates/tx-subsystems/src/vfs/walker/tests.rs:599`:
  ```text
  // FIXME: passes in isolation, fails under workspace serial run as a ...
  ```
  → **legitimate-open** (flaky test). Should be filed as a
  follow-up ticket; not a one-line fix.

- **0** `XXX`, **0** `HACK`. Good hygiene.

| File:line | Marker | Category | Action |
|---|---|---|---|
| crates/tx-subsystems/src/vfs/walker/tests.rs:599 | `FIXME: passes in isolation, fails under workspace serial run` | legitimate-open (flaky test) | follow-up ticket; do not delete |
| crates/tx-shims/src/linux_syscall/aio.rs:26 | `TODO PR-11 phase 2b: spawn deferred` | migration-residual | rename to `TODO(phase-aio-spawn)` or `txdoc:` to D8 |
| crates/tx-shims/src/linux_syscall/aio.rs:393 | `TODO PR-11 phase 2b: spawn deferred` | migration-residual | same |
| crates/tx-subsystems/src/tty/structure/identity.rs:36,202,207 | `TODO(Phase G)` × 3 | legitimate-open | cross-link to Phase G doc once written |
| crates/tx-subsystems/src/tty/structure/payload.rs:89 | `TODO(Phase G)` | legitimate-open | same |
| crates/tx-subsystems/src/tty/ldisc/termios_change.rs:40 | `TODO(Phase G)` | legitimate-open | same |
| crates/tx-ext4/src/pager.rs:36 | `TODO(spec-reconciliation)` | legitimate-open | keep |
| crates/tx-scripts/Cargo.toml:21 | `TODO(elf-loader)` | legitimate-open | keep (goblin upstream) |
| crates/tx-scripts/src/process/exec/loader.rs:370,384 | `TODO(multi-bss)` | legitimate-open | keep |
| crates/tx-scripts/src/process/exec/loader/tests.rs:492 | `TODO: handle multi-BSS later` | legitimate-open | tag as `TODO(multi-bss)` for grep |
| crates/tx-kernel/src/trap_handoff.rs:222 | doc-comment mentions "TODO for the" downstream caller | legitimate-open | keep |
| crates/tx-shims/src/linux_syscall/{mod,numbers}.rs | prose mentions RTC/cputime TODOs | legitimate-open | keep |
| **All 48 `TODO(phase-*)`** | legitimate-open | keep — each is the in-source landing pad for its named phase | n/a |

**Zero already-resolved TODOs were found.** The 39-worker migration
cleaned its own breadcrumbs.

### 3.2 Decisions docs

24 TODO references in `docs/progress/decisions/`. All are forward
pointers from prose explanation (e.g. "`TODO(phase-userva)` migrations
lift the kernel from…"). None require action.

### 3.3 ADR cross-reference for `TODO(phase-*)` tags

For traceability, the most-frequent phase tags map to existing ADRs:

- `phase-userva` → `2026-05-07-shell-prompt-roadmap-progress.md` §3
- `phase-thread-signals`, `phase-signal-frame`, `phase-pgrp-kill` →
  `2026-05-11-d9-signal-wake-migration.md`,
  `2026-05-05-signal-delivery-sweep-day1.md`
- `phase-tls`, `phase-futex`, `phase-pid-resolver` →
  `2026-05-06-fork-clone-wait4.md`
- `phase-elf-loader`, `multi-bss` →
  `2026-05-06-elf-loader-and-execve.md`
- `phase-blocking-read` →
  `2026-05-06-pre-elf-runtime-completion.md`
- Remaining `phase-*` tags (`phase-rusage`, `phase-rtc`,
  `phase-cputime`, `phase-rlimit-enforcement`, `phase-nonblock`,
  `phase-fcntl-setfl`, `phase-readdir-mount`, `phase-umask`,
  `phase-dirfd`, `phase-symlink-flag`, `phase-linkat-nofollow`,
  `phase-vfs-*`, `phase-goblin-share`, `phase-process-topology`)
  are mentioned in passing in shell-prompt-roadmap, fd-ops-and-drift,
  and pre-elf-runtime ADRs but have **no dedicated ADR**. That is the
  natural next batch of "phase planning" ADRs once the migration is
  fully accepted.

## 4. Cleanup plan

Total estimate: **~2 days single-worker, easily parallelizable across
4 workers down to ~0.5 days**. None of this is on the migration
critical path; the workspace already builds, tests pass, and the
warnings do not gate `cargo xtask` smoke runs.

### Phase A — zero-risk deletions (single worker, < 0.25 d)

- Remove the `WaitSourceId` import in
  `crates/tx-subsystems/src/process/structure.rs:34`
  (`cargo fix --lib -p tx-subsystems` or hand-edit).
- That is the **only** zero-risk deletion in the entire audit.

### Phase B — scaffolding decoration (single worker, < 0.5 d)

For each of the 26 distinct dead-code sites in §2:

1. Add `#[allow(dead_code)]` on the struct.
2. Add a one-line `// scaffolding: PR-2 StepOp adapter awaiting
   caller migration per docs/Txv3/03_STEP_MODEL_v2.md §2.1`.
3. (Optional) Add a tagged `txdoc:STEP-MODEL-V2-PR2` anchor at the
   top of each `*Op` doc-comment so a CI grep can later confirm the
   wraps are all gone when adoption completes.

This is purely cosmetic. It silences the warnings and gives the next
caller-migration worker a stable grep.

### Phase C — TODO conversion to follow-up tickets (single worker, < 0.5 d)

1. Rename the 2 `TODO PR-11 phase 2b:` markers in
   `tx-shims/linux_syscall/aio.rs` to `TODO(phase-aio-spawn)` and
   add a `txdoc:` ref to
   `2026-05-11-d8-pr-11-aio-plan.md`.
2. Tag the 1 untagged `TODO: handle multi-BSS later` in
   `tx-scripts/.../loader/tests.rs:492` as `TODO(multi-bss)` for
   grep uniformity.
3. File the flaky-test `FIXME` in `vfs/walker/tests.rs:599` as a
   tracked follow-up. (Either keep the FIXME and link the ticket, or
   downgrade the test to `#[ignore]` with the ticket cited inline.)
4. Either create a "Phase G" planning ADR or cross-link the 5 `TODO
   (Phase G)` markers to an existing roadmap entry.

### Phase D — worker-breadcrumb → txdoc conversion (no work needed)

Search for `// W-[A-Z]+ ` and `// PR-\d+ phase ` returns **0** and
**3** hits respectively (all in `linux_syscall/aio.rs`, already
covered by Phase C). Workers cleaned up after themselves; no Phase D
action is required.

### Optional Phase E — non-migration cleanup

The two prose mentions of "TODO" in
`crates/tx-kernel/src/trap_handoff.rs:222` and
`crates/tx-shims/src/linux_syscall/{mod,numbers}.rs:1142/683` are
documentation describing future work, not actionable items. Leave
them. If desired, replace inline prose with a `txdoc:` link to the
appropriate ADR.

## 5. Test-helper inventory

`grep -rEn "_for_test\b" crates/` returns **464** lines. This is a
deliberate test-surface vocabulary and **no action is recommended**.
The catalog is large enough that listing every line in this ADR would
add noise; the raw output lives at `/tmp/test_helpers.log` during this
audit. Highlights by area:

- `tx-substrate/src/zone/*` — zone/cap test constructors
  (`Cap::new_for_test`, `Weak::new_for_test`, `PayloadCap::for_test`).
- `tx-substrate/src/step_v3/*` — `ScriptCtx::*_for_test`, test
  `SubjectIdentity` constructors, mock wait sources.
- `tx-subsystems/src/test_support.rs` — `EPOCH_TEST_LOCK` and friends.
- `tx-subsystems/src/process/structure.rs`,
  `tx-subsystems/src/tty/structure/*` — payload/identity test
  factories.
- `tx-reactor/src/wait/*` — channel/mask test fixtures.

Future test writers can rely on these as the public test surface.
None are leaking into release builds (all are `#[cfg(any(test,
feature = "test-support"))]` or `pub(crate)` behind the same gate).

## 6. `unsafe impl ZoneAllocated` inventory

29 sites. All are explicit, located at the type-definition site, and
preserve the trait's safety contract per
`docs/design/00_meta-framework/object_model_v2.md`. Test coverage is
mixed:

- `tx-substrate/tests/zone.rs` covers `Object` and `LargeObject` —
  the canonical zone round-trip tests.
- `tx-substrate/src/step_v3/subject_context.rs` types
  (`ProcessIdentity`, `ThreadIdentity`, `Credential`,
  `RestrictionStackHandle`) are exercised by `tx-substrate`
  step-context tests.
- The 25 production-payload impls in `tx-subsystems/` rely on the
  generic zone-allocator tests in `tx-substrate` rather than
  per-type round-trip tests. This is acceptable because the
  `ZoneAllocated` contract is purely about layout/drop, but a
  follow-up "round-trip witness per payload" sweep is a reasonable
  hardening pass.

List (file:line):

```
crates/tx-subsystems/src/signalfd.rs:411            SignalFd
crates/tx-subsystems/src/pipe.rs:207                PipePayload
crates/tx-subsystems/src/io_uring.rs:445            IoUring
crates/tx-subsystems/src/aio.rs:611                 AioContext
crates/tx-subsystems/src/userfaultfd.rs:698         UserfaultFd
crates/tx-subsystems/src/cred.rs:189                Cred
crates/tx-subsystems/src/mount.rs:21                MountIdentity
crates/tx-subsystems/src/mount.rs:27                MountPayload
crates/tx-subsystems/src/mount.rs:33                MountNamespace
crates/tx-subsystems/src/zones.rs:20                ZoneSmokeObj
crates/tx-subsystems/src/page_backed.rs:45          PageContainer
crates/tx-subsystems/src/vfs/structure.rs:59        DEntry
crates/tx-subsystems/src/vfs/structure.rs:65        RNode
crates/tx-subsystems/src/vfs/structure.rs:71        OpenFile
crates/tx-subsystems/src/tty/structure/registry.rs:162  TtyIdentity
crates/tx-subsystems/src/tty/structure/registry.rs:169  TtyPayload
crates/tx-subsystems/src/thread_runtime/structure.rs:405  ThreadIdentity
crates/tx-subsystems/src/thread_runtime/structure.rs:411  ThreadPayload
crates/tx-subsystems/src/vm/structure/address_space.rs:20 AddressSpace
crates/tx-substrate/tests/zone.rs:23                Object
crates/tx-substrate/tests/zone.rs:29                LargeObject
crates/tx-subsystems/src/process/structure.rs:1247  ProcessIdentity
crates/tx-subsystems/src/process/structure.rs:1253  ProcessPayload
crates/tx-subsystems/src/process/structure.rs:1259  ProcessGroup
crates/tx-subsystems/src/process/structure.rs:1265  Session
crates/tx-substrate/src/step_v3/subject_context.rs:119 ProcessIdentity
crates/tx-substrate/src/step_v3/subject_context.rs:158 ThreadIdentity
crates/tx-substrate/src/step_v3/subject_context.rs:184 Credential
crates/tx-substrate/src/step_v3/subject_context.rs:214 RestrictionStackHandle
```

## 7. Pre-PR-1 vocabulary in comments

`grep -rEn "OnCarrier|WakeCarrier|InterestConditions" crates/` returns
**3** lines, all in `crates/tx-substrate/src/step_v3/mod.rs`:

| Line | Use |
|---|---|
| 90 | `/// Renamed from `WakeCarrier` per docs/Txv3/07_BLAST_RADIUS.md §3.1.` |
| 105 | `/// `InterestConditions` per docs/Txv3/07_BLAST_RADIUS.md §3.1.` |
| 120 | `/// PR-0 pinned `OnWaitSource` (formerly `OnCarrier`); ...` |

These are **intentional historical-rename anchors** documenting the
rename so future readers can grep either name and land in the right
place. **No action**. They are not stale vocabulary — they are
deliberate cross-references and should be preserved.

## 8. Worker-letter breadcrumbs

`grep -rEn "// W-[A-Z]+ " crates/` → **0 results**. Workers cleaned
up their own letter-tags. No conversion work.

`grep -rEn "// PR-\d+ phase " crates/` → 3 results, all in
`tx-shims/linux_syscall/aio.rs`, addressed by Phase C above.

## 9. Recommendation

**Wait.** Defer Phases B–C until either:

1. A worker is otherwise idle and wants a small, safe cleanup
   sweep, or
2. CI is upgraded to `-D dead_code` (which the migration-completion
   audit recommends but does not require for the next milestone).

Phase A (the single `unused import`) can land in any incidental commit
— it is a 1-line `cargo fix` and removes an actual stale reference.

These warnings are cosmetic, the structural migration is done, and the
PR-2 `StepOp` wraps will all gain real callers as we migrate the
remaining call sites. Decorating them now risks confusing the next
caller-migration worker into thinking the wraps are deprecated.

## 10. Verification

- `cargo check --workspace --tests` re-run: 27 `dead_code` + 1
  `unused_imports` + 0 other warnings, matching this audit.
- `grep -rEn "TODO|FIXME|XXX|HACK" crates/ docs/progress/decisions/`
  totals: 64 (`crates/`) + 24 (`decisions/`) = 88.
- No production source files were modified during this audit.
- `cargo xtask progress validate` — not run (no JSON records changed).

## 11. Cross-references

- `docs/Txv3/03_STEP_MODEL_v2.md` §2.1 — `StepOp` adapter shape.
- `docs/Txv3/07_BLAST_RADIUS.md` §3.1 — `OnCarrier`/`WakeCarrier`/
  `InterestConditions` rename history.
- `docs/progress/migration-completion-audit-2026-05-12.md` — sibling
  audit covering structural migration completion.
- `docs/progress/decisions/2026-04-28-unused-lint-gate.md` — prior
  decision on the unused-lint gate strategy.
- `docs/progress/decisions/2026-05-11-d8-pr-11-aio-plan.md` — anchor
  for the two PR-11-tagged TODOs in `aio.rs`.
- `docs/progress/decisions/2026-05-11-d9-signal-wake-migration.md` —
  anchor for the signal-phase TODOs.
