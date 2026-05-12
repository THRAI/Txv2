# Decision D5: PR-9 phase 5 — zone-allocate `Cred` for `Cap<Cred>` minting

**Date:** 2026-05-11
**Status:** decided
**Worker:** W-I (research-only)
**Companion:** [D1](2026-05-11-d1-scriptctx-trait-bound-identity.md)
(trait-bound subject identity),
[D4](2026-05-11-d4-bus-mailbox-layering.md) (precedent ADR style).
**Successor of:** the "phase 5 follow-up" note recorded at
`docs/progress/STATUS.md` 2026-05-11 v3 PR-9 phase 4 LANDED entry —
that note flagged the blocker; this ADR chooses the resolution.

## 1. Problem statement

PR-9 phase 4 (LANDED 2026-05-11) reshaped `SubjectContext<I>` and
`SubjectAuthority<I>` so they store **caps** rather than values
(`crates/tx-substrate/src/step_v3/subject_context.rs:259, 324`):

```rust
pub struct SubjectAuthority<I: SubjectIdentity = ProcessIdentity> {
    cred: Cap<I::Credential>,
    restrictions: Cap<I::Restrictions>,
}

pub struct SubjectContext<I: SubjectIdentity = ProcessIdentity> {
    process: Cap<I>,
    thread: Option<Cap<I::ThreadIdentity>>,
    authority: SubjectAuthority<I>,
}
```

Phase 5 plans to have each syscall arm populate one of these from its
`SyscallCtx<'a>` (`crates/tx-shims/src/linux_syscall/mod.rs:285`) at
entry, so step bodies that need authority receive `&SubjectContext`
explicitly per SUBJ-1.

**Blocker.** Production `Cred` does not live in a zone today. It lives
inside `ProcessPayload`'s **`SpinMutex<Cred>`** field
(`crates/tx-subsystems/src/process/structure.rs:688`):

```rust
pub(crate) cred: SpinMutex<Cred>,
```

There is no `Cap<Cred>` anywhere in the workspace. `SyscallCtx::cred()`
returns `Cred` *by value* (snapshot-copy through the lock); there is no
cheap way to mint a `Cap<Cred>` for `SubjectAuthority::new(...)`.

**Constraints.**

- `Cap<T>` requires `T: 'static` (already true of `Cred`) and that `T`
  implements `ZoneAllocated` with a registered `Zone<T>`. Today `Cred`
  does neither.
- `setuid`-family mutations must be atomic *as viewed from any
  concurrent syscall arm* (no torn reads of a partially-updated cred —
  the cred is a 8-field POSIX tuple, and an authority check that sees
  the new uid but the old caps is a real privilege bug).
- Phase 5 minting must be cheap (sub-microsecond) because the
  `SubjectContext` is materialized at *every* syscall entry that needs
  authority (today: 25 sites in `tx-shims/linux_syscall/`, see §4).

## 2. Three paths surveyed

### Path A — zone-allocate `Cred`; install fresh `Cap<Cred>` per mutation (COW)

**Sketch.** Replace `ProcessPayload.cred: SpinMutex<Cred>` with a
slot-style field holding the *current* cap. Each mutator (`setuid`,
`setgid`, `setresuid`, `setresgid`, `setreuid`, `setregid`,
`apply_suid_for_exec` — 7 sites in `crates/tx-subsystems/src/cred.rs`,
plus the 3 test helpers `clear_caps_for_test` / `install_caps_for_test`
/ `set_cred_ids_for_test`) reads the current cap, computes the new
`Cred`, reserves a fresh zone slot, signs it into a new `Cap<Cred>`,
and atomically swaps the slot. The previous cap is dropped via EBR —
any syscall arm still holding the old `Cap<Cred>` reads the
pre-mutation cred for the duration of its borrow.

**`setuid`-family atomicity.** A `setuid` is one allocation +
`AtomicSlot::swap(Some(new_cap))`. Pre-mutation readers see the old
cred; post-swap readers see the new cred. There is no torn-read window
because the cred is read through a *single* cap pointer — the old
8-field `SpinMutex<Cred>` two-phase "read, decide, write" goes away
entirely. Atomicity is the swap; visibility is `release/acquire` on
the slot. Privilege-rule checks (`prev.is_privileged_for(...)`) still
run against the *current* cap loaded once at function entry.

**Storage cost.** One zone slot per live process + one per
in-flight authority borrow that outlives a setuid (rare; bounded by
concurrent syscalls). At ~1k processes × ~32 B per `Cred` = 32 KiB
zone-resident, indistinguishable from today's per-payload
`SpinMutex<Cred>` cost. Cap-key churn: one new cap per
cred-mutation syscall (`setuid` etc. are ~0.01% of syscalls on a
typical workload — negligible).

**Hot-path read cost.** Each `SubjectContext` materialization does
`payload.cred_slot.load()` returning a `Cap<Cred>` clone (one lock
acquisition on `AtomicSlot`, one atomic retain-count bump on the cap
key). Compares favorably to today's `payload.cred.lock(); *guard`
(one lock acquisition, one 32-byte copy). **Net: same lock cost,
similar memory traffic, with the advantage that the resulting cap is
shareable across the script frame without re-locking.**

**Migration cost.** ~7 mutators rewrite their lock-and-update body
into reserve/sign/swap. Each is ~5 LoC changed. The 3 test helpers
gain the same shape. `ProcessPayload.cred` field type changes;
`ProcessPayload::cred(&self) -> Cred` accessor becomes `cred_cap(&self)
-> Cap<Cred>` (existing callers that want a value snapshot
`*cap.lock()` or similar — but `Cap<Cred>` `Deref`s to `&Cred`, so
most callers stay value-shaped via `*cap`). `bootstrap_init_process` /
`step_fork` (the 2 cred-construction sites) reserve+sign a fresh
`Cap<Cred>` instead of wrapping in `SpinMutex::new`.

**Testing surface.** 29 `#[test]` blocks in
`crates/tx-subsystems/src/cred/tests.rs` (610 LoC). The two helpers
`cred_of(&proc_cap)` and `set_cred(&proc_cap, cred)` at the top of
that file (`tests.rs:43-51`) directly poke
`payload.cred.lock()` — they must move to the new cap shape (use
`payload.cred_cap()` and `payload.replace_cred_cap(new)`). The
remaining 27 tests exercise behavior through the public `step_*`
mutators and are insulated.

**Future flex (landlock/seccomp restriction-cell).** Once
`SubjectAuthority<I>.restrictions` lands a real
`Cap<RestrictionStack>`, the same idiom (slot-style cap, swap on
publication boundary per SUBJ-3 suid-exec) generalizes directly.
**Cred and restrictions are isomorphic under this path.**

### Path B — keep `SpinMutex<Cred>`; mint a fresh `Cap<Cred>` per syscall entry (snapshot-handle)

**Sketch.** `ProcessPayload.cred: SpinMutex<Cred>` is unchanged. At
syscall entry, `SyscallCtx::subject_context()` does:

```rust
let cred_value = self.process.cred().unwrap_or_else(Cred::root);  // *self.cred.lock()
let reservation = zone::reserve_for::<Cred>()?;
let cred_cap = zone::sign_for(reservation, cred_value);
// build SubjectContext with this single-use cred_cap
```

Each syscall mints its own cap; the cap is dropped at script frame
exit (EBR-deferred drop).

**`setuid`-family atomicity.** Unchanged from today: lock-decide-write
under `SpinMutex<Cred>`. The mutator's atomicity is independent of
the cap minting — caps are read-only snapshots.

**Storage cost.** **One zone allocation per syscall** that needs
authority — minimum 25 sites today, projected to grow as more
syscalls land. At ~1M syscalls/sec workload that is 1M cred-zone
allocations/sec churn, each followed by an EBR retire. This
fundamentally changes the cred zone from "1 entry per process" to "1
entry per in-flight syscall" — and the zone slab pressure goes from
O(processes) to O(syscalls/sec × syscall_duration). Even if each cap
is sub-microsecond to retire, the slab depth must exceed the maximum
in-flight syscall count or the reserve fails. **This is a regression
of one to two orders of magnitude on cred-zone slab pressure.**

**Hot-path read cost.** Per syscall: one `SpinMutex<Cred>` lock (same
as today) **plus** one `reserve_for::<Cred>()` (zone-slot allocation,
typically a couple atomics on the slab) **plus** one `sign_for` (a
write into the slot and a retain-count init). Approximately 2-3× the
current `SyscallCtx::cred()` cost — measurable on a microbenchmark,
acceptable but unnecessary.

**Migration cost.** Smallest: zero changes to the 7 `setuid` mutators
or to the 3 test helpers. One new accessor on `SyscallCtx`
(`cred_cap_snapshot()`) and one zone registration for `Cred`. ~30
LoC.

**Testing surface.** Zero existing tests touched. The 27 tests in
`cred/tests.rs` keep their current `payload.cred.lock()` poke pattern.

**Future flex.** Snapshot-handle works in principle for restrictions
too — but `RestrictionStack` is **append-only with a `Vec<RestrictionKind>`
backing** (`crates/tx-substrate/src/step_v3/restriction_stack.rs:55`),
so snapshotting it per-syscall means cloning the whole vec each time.
That cost compounds with restrictions depth (seccomp filters can be
hundreds of rules). **Restrictions cannot adopt this pattern
economically; the asymmetry is a strong negative.**

### Path C — zone-allocate `Cred` + `SpinMutex<Cap<Cred>>` slot; mutators swap caps atomically

**Sketch.** Like Path A, `Cred` becomes zone-allocated. But instead of
exposing a slot-style `AtomicSlot<Cap<Cred>>`, the cell type is
`SpinMutex<Cap<Cred>>` — equivalent to today's `SpinMutex<Cred>` but
the inner value is a cap. Readers do `*payload.cred.lock()` and get a
`Cap<Cred>` clone; mutators lock, compute new `Cap<Cred>`, swap, drop
old (EBR-retire on drop).

**`setuid`-family atomicity.** Single lock acquisition over a
`Cap<Cred>` swap (cap clone + lock + swap + drop). Atomicity matches
Path A — readers see either the old or the new cap.

**Storage cost.** Same as Path A — one zone slot per live process,
churn on mutation. Same number of in-flight cap entries during EBR
grace periods.

**Hot-path read cost.** One `SpinMutex` lock + one `Cap::clone` (one
atomic retain-count increment) per `SubjectContext` materialization.
Same shape as Path A's `AtomicSlot::load()` once Path A's slot lowers
to `SpinMutex` (which `AtomicSlot` is today —
`crates/tx-substrate/src/slot.rs:19-21`). **Path A and Path C have
identical hot-path read cost today.** They diverge only if/when
`AtomicSlot` migrates to a real lock-free implementation; Path A
benefits, Path C does not.

**Migration cost.** Equivalent to Path A. The slot type spelling
differs (`SpinMutex<Cap<Cred>>` vs `AtomicSlot<Cap<Cred>>`), but the
mutator bodies, the constructor sites, and the test-helper rewrites
are identical.

**Testing surface.** Same as Path A.

**Future flex.** Same as Path A for restrictions. Slightly worse
forward-compat: when `AtomicSlot<Cap<T>>` becomes lock-free,
`SpinMutex<Cap<T>>` does not benefit. Path C is "Path A as written
today" and Path A is "Path A as it will eventually be."

## 3. Current cred mutability survey

### 3.1 `SpinMutex<Cred>` declarations

Exactly **one declaration site**:

```
crates/tx-subsystems/src/process/structure.rs:688
    pub(crate) cred: SpinMutex<Cred>,
```

The field is `pub(crate)` and consumed both via the `ProcessPayload::cred()`
accessor (returns `Cred` by Copy) at `structure.rs:839` and by direct
`payload.cred.lock()` from `crates/tx-subsystems/src/cred.rs`.

### 3.2 Mutation sites in `crates/tx-subsystems/src/cred.rs`

**7 mutating step functions** (each acquires `payload.cred.lock()` and
mutates the guard in place under a `SeqCst` fence):

| Step fn | Line | Surface |
|---|---|---|
| `step_setuid` | `cred.rs:212` | scalar `Uid` |
| `step_setgid` | `cred.rs:247` | scalar `Gid` |
| `step_setresuid` | `cred.rs:282` | triple `Option<Uid>` |
| `step_setresgid` | `cred.rs:337` | triple `Option<Gid>` |
| `step_setreuid` | `cred.rs:398` | pair `Option<Uid>` |
| `step_setregid` | `cred.rs:450` | pair `Option<Gid>` |
| `step_apply_suid_for_exec` | `cred.rs:562` | binary mode + file ids |

Each follows the identical pattern (`cred.rs:217-238` representative):

```rust
let payload_guard = target.payload.lock();
let Some(payload) = payload_guard.as_ref() else { return CredChange::Zombie; };
let mut cred_guard = payload.cred.lock();      // ← mutation point
let prev = *cred_guard;
let mut new = prev;
// ... privilege rule, decide `new` ...
*cred_guard = new;
drop(cred_guard);
drop(payload_guard);
core::sync::atomic::fence(Ordering::SeqCst);   // force-publish
```

**3 test-only helpers** (cfg-gated, same pattern):

| Helper | Line | Purpose |
|---|---|---|
| `clear_caps_for_test` | `cred.rs:684` | zero `effective_caps` / `permitted_caps` |
| `install_caps_for_test` | `cred.rs:705` | install narrow cap set |
| `set_cred_ids_for_test` | `cred.rs:726` | overwrite all 6 uid/gid fields |

### 3.3 Construction sites

**1 cred-cell construction site**: `process/execution.rs:985`
inside `sign_process_payload`. Called from `bootstrap_init_process`
(passes `Cred::root()`) and from `step_fork` (passes
`parent.payload.cred()` — Copy of parent). Both sites pass `Cred` by
value; phase 5 swaps `cred: SpinMutex::new(cred)` for either
`cred: AtomicSlot::with_value(sign_cred(cred))` (Path A) or
`cred: SpinMutex::new(sign_cred(cred))` (Path C).

### 3.4 Locking discipline today

- Two-layer lock: outer `payload: SpinMutex<Option<PayloadCap>>` for
  zombie discrimination, inner `cred: SpinMutex<Cred>` for atomicity.
  Drop order: inner first, outer second.
- A `core::sync::atomic::fence(Ordering::SeqCst)` follows every
  mutation to publish the new cred to concurrent readers that may be
  holding `Cred` snapshots on other cores.
- Readers (`ProcessPayload::cred()`, `ProcessIdentity::cred()`,
  `SyscallCtx::cred()`) take the lock, copy out `*guard`, drop the
  lock — `Cred: Copy` so the snapshot is independent.

## 4. Syscall hot-path cred-read survey

`grep -rEn 'ctx\.cred\(\)|ctx\.walker_cred\(\)'
crates/tx-shims/src/linux_syscall/ | grep -v /tests` yields **23
call sites** (24 with the `mod.rs` doctext mentions discarded), spread
across:

- `linux_syscall/cred.rs`: 7 sites (4 `ctx.cred()` for the `getuid`
  family; 3 inside `sys_getres*id` for the readback triple).
- `linux_syscall/fs_mut.rs`: 8 sites — every `chmod`/`chown`/`utime`
  arm uses `walker_cred()`.
- `linux_syscall/fs_path.rs`: 4 sites — `chmodat`/`chownat`/`faccessat`
  (`fs_path.rs:200, 242, 321, 438`).
- `linux_syscall/fs_basic.rs`: 2 sites — `sys_openat`
  (`fs_basic.rs:207`) and `sys_readlinkat` (`fs_basic.rs:941`).
- `linux_syscall/proc.rs`: 1 site — `sys_execve` (`proc.rs:124`).

**The 7 canonical PR-9 syscalls** (from W-A's wired set):

| Syscall | File:line | Cred read on hot path? |
|---|---|---|
| `sys_open` (`sys_openat`) | `fs_basic.rs:154` | **Yes** — `walker_cred()` at line 207 for DAC walk. |
| `sys_read` | `io.rs:347` | **No** — fd-resolution only; the DAC was checked at `open` time and the resulting `OpenFile` carries the access mode. |
| `sys_write` | `io.rs:192` | **No** — same reasoning; `io.rs:192-211` reads only `ctx.process` for fd lookup. |
| `sys_fork` (`sys_clone`) | `mod.rs:410` (NR_CLONE → `sys_clone::<P>`) | **No** — `step_fork` reads `parent.payload.cred()` *inside* the step body to seed the child's cred (`process/execution.rs:322`); the syscall arm itself does not pre-snapshot. |
| `sys_execve` | `proc.rs:77` | **Yes** — `walker_cred()` at line 124 to drive `exec_script`'s DAC walk + suid recompute. |
| `sys_close` | `fs_basic.rs:344` | **No** — fd-resolution only. |
| `sys_pipe2` | `fs_basic.rs:443` | **No** — allocates two fds; no path walk, no DAC. |

**3 of 7 hot-path read cred.** This is *less* than half — the
`SubjectContext` materialization cost for `read`/`write`/`close`/`pipe2`
is pure overhead (no consumer in their step bodies) unless those arms
gain authority-bearing step bodies later. The phase 5 plan should
acknowledge that materialization is **prophylactic** at half the
syscalls: cheap enough to be worth the SUBJ-1 hygiene win, but the
hot-path microbenchmark must validate that "cheap enough" is met.

## 5. Recommendation — **Path A**

**Zone-allocate `Cred`; `ProcessPayload.cred: AtomicSlot<Cap<Cred>>`;
mutators reserve+sign+swap.**

Rationale weighing the four costs:

1. **Correctness.** Path A naturally enforces the existing
   cred-mutation invariants. Atomicity becomes a slot swap (one
   atomic with `release/acquire`); no torn reads are possible
   because there is no intermediate "old uid + new caps" state — the
   *whole* `Cred` is replaced at a single pointer swap. The
   `SeqCst` fence sprinkled after every today-mutator (`cred.rs:238,
   270, 330, 385, 443, 493, 597`) collapses into the slot store's
   release semantics. **Path B keeps today's `SpinMutex` discipline
   verbatim, which is also correct but does not simplify;** Path C
   matches Path A on correctness.

2. **Performance.** Path A's hot-path read is `AtomicSlot::load()` →
   `Option<Cap<Cred>>` clone — one lock + one atomic increment. Path
   C is identical today but loses the lock-free-slot upgrade path
   (`crates/tx-substrate/src/slot.rs` doctext explicitly anticipates
   this). Path B is **2-3× slower** on the cred read path and
   imposes O(syscalls/sec) churn on the cred zone slab. **Path A
   wins by being the same speed as Path C today and strictly faster
   later, while Path B regresses.**

3. **Migration cost.** Path A and Path C touch the same ~10
   files: `cred.rs` (7 mutators + 3 test helpers), `process/structure.rs`
   (1 field swap + 1 accessor), `process/execution.rs` (1
   constructor), `zones.rs` (1 register). Path B touches `mod.rs`
   (1 helper) and zone registration — half the diff — but the
   cognitive cost is hidden in the *eventual* migration when
   restrictions land and the asymmetry forces a Path A rewrite of
   cred anyway.

4. **Future flex.** Restrictions are the load-bearing test of
   "future flex." Today's `RestrictionStackHandle` in `step_v3`
   (`subject_context.rs:208`) is a placeholder; the real
   append-only `RestrictionStack`
   (`step_v3/restriction_stack.rs:55`) is a `Vec<RestrictionKind>`.
   Per SUBJ-3 the cred-and-restrictions authority is replaced as a
   single publication boundary (suid-exec, no-new-privs flip); both
   members of `SubjectAuthority<I>` *must* share a cap-swap shape.
   **Path A's cred-slot-and-swap idiom drops directly into the
   restriction landing pad; Path B does not.**

**Trade-off acknowledged.** Path A increases the surface area of
`ProcessPayload`'s field shape (one `AtomicSlot<Cap<T>>` for the cred,
matching the existing `aspace: AtomicSlot<Cap<AddressSpace>>` field at
`structure.rs:675`). This is a *positive*: cred joins aspace as a
field replaceable by atomic-slot swap, which is the same idiom exec
already uses for the address-space visibility boundary
(`txdoc:EXEC-11-PHASE-6-ADDRESS-SPACE-VISIBILITY-BOUNDARY`). **One
idiom, two fields, no cognitive shear.**

## 6. PR-9 phase 5 implementation plan (Path A)

### 6.1 File-level changes

| Order | File | Change | LoC |
|---|---|---|---|
| 1 | `crates/tx-subsystems/src/cred.rs` | Add `unsafe impl ZoneAllocated for Cred` against a new `static CRED_ZONE: Zone<Cred>`. Implements `tx_substrate::zone::ZoneAllocated::zone()`. | +6 |
| 2 | `crates/tx-subsystems/src/zones.rs` | Add `cred::register_zones()` and call it from `register_all()`. Register `Cred` alongside the existing process/thread/vm zones. | +6 |
| 3 | `crates/tx-subsystems/src/process/structure.rs` | `cred: SpinMutex<Cred>` → `cred: AtomicSlot<Cap<Cred>>`. Replace `ProcessPayload::cred(&self) -> Cred` with `cred_cap(&self) -> Cap<Cred>` (returns the loaded cap, panic on empty slot — by construction always populated). Add `replace_cred(&self, new: Cap<Cred>) -> Cap<Cred>` calling `self.cred.swap(Some(new))`. | ~+20 / -3 |
| 4 | `crates/tx-subsystems/src/process/execution.rs` (`sign_process_payload`) | Replace `cred: SpinMutex::new(cred)` with `cred: { let s = AtomicSlot::empty(); s.store(Some(sign_cred(cred)?)); s }`. Add `fn sign_cred(c: Cred) -> Result<Cap<Cred>, ZoneError>` helper. The 2 cred-construction call sites (`bootstrap_init_process` and `step_fork`) keep passing `Cred` by value. | ~+15 / -1 |
| 5 | `crates/tx-subsystems/src/cred.rs` | Rewrite the 7 mutators. Each body becomes: load current cap (`payload.cred_cap()`), `Deref` to `&Cred` for the privilege rule, build `new: Cred`, `let new_cap = sign_cred(new)?`, `payload.replace_cred(new_cap)`. The dropped old cap is EBR-deferred — no explicit fence required (the slot swap publishes via release). | ~140 (rewrite the bodies) |
| 6 | `crates/tx-subsystems/src/cred.rs` (test helpers) | Same shape as the mutators for `clear_caps_for_test` / `install_caps_for_test` / `set_cred_ids_for_test`. | ~40 |
| 7 | `crates/tx-subsystems/src/cred/tests.rs` | Update `cred_of(&proc_cap)` and `set_cred(&proc_cap, cred)` helpers at `tests.rs:43-51` to use the new accessor / mutator. 27 downstream tests stay unchanged. | +5 / -8 |
| 8 | `crates/tx-subsystems/src/process/structure.rs` (`ProcessIdentity::cred`) | Returns `Option<Cred>` today (`structure.rs:285`). Keep this surface for value callers (`getuid`, etc.) but add `cred_cap(&self) -> Option<Cap<Cred>>` for phase 5 minting. | +6 |
| 9 | `crates/tx-shims/src/linux_syscall/mod.rs` (`SyscallCtx`) | Add `cred_cap(&self) -> Cap<Cred>` — clones from `process.cred_cap()` with a `Cred::root()` fallback signed on the fly for zombies (defensive, mirrors today's `cred()`). | +20 |
| 10 | `crates/tx-shims/src/linux_syscall/mod.rs` (each of W-A's wired arms) | At entry, build `let subject = SubjectContext::from_thread(ctx.process.clone(), ctx.thread.clone(), SubjectAuthority::new(ctx.cred_cap(), restrictions_cap_placeholder))`. Thread `&subject` through to the step body. **Restrictions cap is a placeholder until that work lands — mint a `Cap<RestrictionStackHandle>` from the substrate placeholder zone.** | ~+30 |

**Total: ~10 files, ~300 LoC.**

### 6.2 Landing sequence (preserve M1 fanout)

The 7 `setuid`-family `StepOp` wraps from PR-9 phase 3a's M1 fanout
(`SetuidOp`, `SetgidOp`, `SetreuidOp`, `SetresuidOp`, `SetresgidOp`,
`SetregidOp`, `ApplySuidForExecOp` at `cred.rs:766-931`) **delegate
to the free `step_*` fns unchanged**. As long as the free fn surface
stays the same (`pub fn step_setuid(target: &Cap<ProcessIdentity>,
new_uid: Uid) -> CredChange`), the wraps compile and behave identically.
**Phase 5 must preserve the free-fn signatures verbatim** — the
zone-allocation lives inside the bodies. Verified by file inspection:
each wrap forwards `(target, args...)` to the free fn at
`cred.rs:780, 800, 822, 847, 873, 897, 924`.

Suggested PR sequencing:

1. **Phase 5a** (1 day, 2 files): land the `Cred` zone registration
   and `sign_cred` helper. Tests: one round-trip test in
   `cred/tests.rs` that signs a `Cred`, reads it back via `Cap`
   `Deref`, retires the cap, and asserts the slab returns to a
   known depth. Zero behavior change to the 7 mutators or any
   downstream syscall arm — `Cred` is now `ZoneAllocated` but
   `ProcessPayload` still uses `SpinMutex<Cred>`.

2. **Phase 5b** (1.5 days, ~8 files): swap
   `ProcessPayload.cred` to `AtomicSlot<Cap<Cred>>`. Rewrite the 7
   mutators to slot-swap. Rewrite the 3 test helpers. Adjust the
   `cred/tests.rs` poke helpers. Cargo check + run the 29 cred tests
   + the 7 `step_op_wraps` tests (`cred.rs:933`). The DAC + setuid
   slice tests in `tx-shims` exercise the live mutation paths — they
   must pass unchanged because `step_setuid(target, uid)` still
   returns `CredChange::Replaced { prev, new }` with the same
   semantics.

3. **Phase 5c** (1 day, ~3 files): add `SyscallCtx::cred_cap()`,
   thread `SubjectContext` materialization into the 7 canonical
   syscall arms. The 4 W-A wired arms get the full
   `SubjectContext::from_thread` build; the 3 unwired
   (`sys_openat`, `sys_execve`, `sys_pipe2` — currently used the
   value-shape `ctx.cred()` / `ctx.walker_cred()`) keep their
   existing accessors and additionally receive the `SubjectContext`
   for SUBJ-1 hygiene. **The 25 `ctx.cred()` / `ctx.walker_cred()`
   call sites surveyed in §4 do not need to change** — phase 5 is
   purely *additive* on the syscall arm surface; the
   `SubjectContext` joins the existing value accessors, it does not
   replace them.

**Total: 3.5 days**, comfortably within the phase 5 budget given the
read-only nature of the consumer migration.

### 6.3 Tests added

- 1 zone-round-trip test in `cred/tests.rs`: sign, deref, retire, depth.
- 1 `setuid` atomicity test: spawn two threads, one calling
  `step_setuid(target, uid_a)` repeatedly, the other reading
  `target.cred_cap()` and asserting the read cap dereferences to either
  the pre-mutation `Cred` or the post-mutation `Cred` (never a torn
  combination). Today's `SpinMutex<Cred>` already guarantees this; the
  test pins the property under the new shape.
- 1 EBR test: hold an old `Cap<Cred>` across a `step_setuid` call;
  assert the old cap still `Deref`s to the pre-mutation cred under a
  live `epoch::Guard`.
- The existing 29 tests in `cred/tests.rs` and 7 in `cred::step_op_wraps`
  re-run unchanged (modulo the 2-line `cred_of` / `set_cred` helper
  update).

**Net: +3 tests, ~0 behavioral changes.**

## 7. Phase 5 prerequisite beyond Cred — restrictions has the **same** shape issue

`SubjectAuthority<I>.restrictions: Cap<I::Restrictions>` is, for
production `ProcessIdentity`, `Cap<step_v3::RestrictionStackHandle>`
(per `process/structure.rs:186`). Today `RestrictionStackHandle` is a
**substrate-side placeholder** (`subject_context.rs:208`) — a
unit-typed struct already implementing `ZoneAllocated` against
`PLACEHOLDER_RESTRICTIONS_ZONE`. **The placeholder works for phase 5
out of the box** — `SyscallCtx::restrictions_cap_placeholder()` can
mint a fresh `Cap<RestrictionStackHandle>` from the placeholder zone
the same way phase 4's tests do (`tests/v3_subject_context.rs:60`).

**However**, the real `RestrictionStack` type (currently
`crates/tx-substrate/src/step_v3/restriction_stack.rs:55`,
`tx-policy`-bound per `structure.rs:177-182`) when it lands will face
the *same* "production value lives inside a mutable container, can't
be moved into a `Cap`" problem. The append-only mutation invariant
(`restriction_stack.rs:67-70`) means the natural shape is the same as
Path A: `Cap<RestrictionStack>` per-process, swap on `append`
(append-only — so the new cap is a `prev + 1 entry` clone, EBR-retire
the old). The phase 5 plan should **document Path A as the shape both
`Cred` and the eventual real `RestrictionStack` adopt** so that the
restrictions landing in PR-K mirrors this ADR rather than re-deriving
it.

No phase 5 *blocker* in restrictions: the placeholder is good enough.
**Flag for PR-K**: the cred-mutation shape this ADR adopts is the
canonical SUBJ-3 publication-boundary idiom; restrictions adopts the
same shape when it lands.

## 8. Relationship to existing ADRs and references

- **D1** (this directory's
  `2026-05-11-d1-scriptctx-trait-bound-identity.md`) declared
  `SubjectIdentity::Credential: CredentialView` and the trait-bound
  shape. This ADR makes `Credential = tx_subsystems::cred::Cred` into a
  zone-allocated entity so the Cap-shape D1 anticipated becomes
  buildable in production. **D1 unchanged.**
- **`docs/Txv3/04_SYSCALL_SHAPE_v1.md` §2** declares
  `SubjectAuthority { cred: Cap<Credential>, restrictions:
  Cap<RestrictionStack> }`. This ADR is the implementation lane to
  make that shape materializable from `SyscallCtx<'a>` at syscall
  entry. **Spec unchanged.**
- **PR-9 phase 4 STATUS entry (2026-05-11)** flagged "production
  `Cred` lives inside `ProcessPayload`'s `SpinMutex<Cred>` rather than
  its own zone — phase 5 needs a `cred_cap()` accessor or a small
  `Cred` extraction PR before the wiring becomes purely mechanical."
  This ADR chooses the `Cred` extraction lane (the "small extraction"
  variant — ~10 files, ~300 LoC).
- **Existing precedent in tree**: `ProcessPayload.aspace:
  AtomicSlot<Cap<AddressSpace>>` (`structure.rs:675`) with exec's
  phase 6 atomic store. This ADR adopts the **same idiom** for cred.
  The cognitive load on a reader of `ProcessPayload` does not
  increase: aspace and cred become structurally parallel fields.
