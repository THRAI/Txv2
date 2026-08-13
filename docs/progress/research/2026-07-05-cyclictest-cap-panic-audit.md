# cyclictest SMP4 Cap panic audit

Date: 2026-07-05

Scope: classify the `cyclictest-glibc` SMP4 `NO_STRESS_P8` panic from the last
run log, without starting a new QEMU run.

Primary log:

- `target/oscomp/smp4-nonltp-20260702/fix-soname-cp/cyclictest-glibc.log`

## Symptom

`cyclictest-glibc` reaches `NO_STRESS_P1` successfully, starts
`NO_STRESS_P8`, prints eight CPU-mask warnings, then panics:

```text
txkernel:panic: panicked at /Users/3y/Downloads/Tx/crates/tx-substrate/src/zone/cap.rs:349:14:
zone Cap key no longer resolves to a live slot
```

`cargo xtask fault-decode --target rv64-qemu --serial
target/oscomp/smp4-nonltp-20260702/fix-soname-cp/cyclictest-glibc.log --all
--brief` maps the trap to `rust_begin_unwind`, confirming a Rust panic path
rather than a hardware memory fault. The saved frame only reaches
`Option::expect`, so this log does not identify the monomorphized `Cap<T>`
type.

At panic shutdown:

- `epoch=4977`, `guards=1`, `zones=50`, `captured=50`; the registry is still
  globally alive.
- live processes are `sh`, `busybox`, and `cyclictest`; `cyclictest` has 2
  live threads.
- high-churn zone summaries include `ThreadPayload` with `slabs=14:empty=1`,
  `ProcessPayload` with `slabs=16:empty=1`, and `AddressSpace` with
  `slabs=22:empty=1`.

## Classification

Current evidence does not support a primary bug in the packed `SlotMeta` CAS
logic.

`Cap::clone`, `Cap::drop`, and `IdentRef::to_cap` all operate on one packed
`SlotWord` containing generation, state, and retain count. That matches
`ZONE-6` and `ZONE-7`: normal races should make weak/ident upgrades fail, not
produce a held `Cap` whose slot later disappears.

The panic is earlier than a metadata mismatch. `Cap::deref()` calls
`registry::slot_for::<T>(key)` and panics if the encoded key cannot be resolved
back to a slot. `registry::slot_for` still checks zone id and `TypeId`; the
zone registry itself remains populated at panic shutdown. Therefore this is a
per-zone key/slab resolvability failure, not a global registry loss.

The slab/Keg audit also does not show a valid-retained-Cap trim path:

- `Keg::slot_from_key` searches linked partial/full/empty slabs, with a checked
  slab cache.
- `Keg::return_slot_inner` retires only slabs whose slab-local free count equals
  the slab slot count.
- per-CPU bucket slots are marked claimed in the Keg bitmap, so remote CPU
  buckets should prevent a slab from looking empty rather than causing false
  empty-slab retirement.
- EBR reclaim returns a slot via `return_slot_from_reclaim`, which skips nested
  whole-slab retirement; later maintenance can trim only after the slot is back
  in the Keg free list.

For a real `Cap<T>` still held by Rust ownership, its slot should not reach
`Free`, should not return to the Keg free list, and should not make its slab
eligible for empty-slab trim. The observed panic is therefore more consistent
with a stale or unretained Cap-shaped handle being used after the last real
retainer dropped, or with a release-mode path using a Cap after its owning
semantic object was torn down.

The important boundary is that `Cap::deref()` fails before it can inspect
`SlotMeta`: `Cap::slot()` calls `registry::slot_for::<T>(key)`, which routes
through the typed registry entry to `Zone::slot_from_key()` and then
`Keg::slot_from_key()`. A `None` here means the encoded zone/slab/slot key no
longer maps to a linked slab, or the key is being resolved through the wrong
typed zone. It is not evidence of a retain/generation/state mismatch inside the
slot word.

The Keg path narrows the shape further. `Keg::slot_from_key()` first checks the
validated slab cache and then walks the partial/full/empty slab lists. Slabs
are unlinked only when the slab-local free bitmap says every slot is free.
That bitmap is changed by reserve/reclaim return paths, not by ordinary cap
lookup. Therefore a live retained `Cap` can become unresolvable only if some
other path already allowed its slot to be returned as free, or if the handle was
not a real retained `Cap` in the first place. Both are lifetime/concurrency
bugs above the meta word, not a normal `SlotMeta` CAS race.

## Suspect shape

The old log is insufficient to name the exact type. Static inspection suggests
these are the first suspects:

1. `ThreadPayload` or `ThreadIdentity`, because `cyclictest NO_STRESS_P8`
   exercises clone/exit/preemption and the shutdown summary shows high
   `ThreadPayload` slab churn.
2. `ProcessPayload` or `AddressSpace`, because syscall dispatch repeatedly
   snapshots them and both are high-churn in the same run.
3. fd/path objects only if a later typed trace points there; current log has
   no file/path-specific marker near the panic.

The per-hart userspace payload slots remain a correctness risk for wrong-owner
handoff if they are not cleared on all exits, but by themselves they should
retain `PayloadCap<ThreadPayload>`. A stale userspace slot containing a real
`PayloadCap` would leak or misroute; it should not make the backing key
unresolvable.

The current thread path is still the highest-probability owner if the missing
type turns out to be `ThreadPayload`: `sign_thread()` signs a `ThreadPayload`,
wraps it in `PayloadCap`, stores it in `ThreadIdentity.payload`, and the boot
reactor submits `PerHartSlotted::new(thread, payload, run_thread(thread,
payload))`. During each poll, `PerHartSlotted` clones the thread and payload
into per-hart current slots, and `enter_userspace_once()` separately clones the
payload into the userspace-running slot for trap handoff. `step_thread_exit()`
and `step_exit_group()` clear `ThreadIdentity.payload` through
`set_thread_zombie()`, but that should only remove the identity-to-payload
lookup; task-future and per-hart clones should continue retaining the payload.
If a typed trace proves `ThreadPayload`, inspect this handoff/exit/preemption
sequence for an unretained copy or task-drain ordering bug, not the packed
`SlotMeta` arithmetic first.

## Next evidence

Do not change lifetime logic from this log alone. The next bounded run should
temporarily instrument the failing lookup path to print:

- `core::any::type_name::<T>()`
- raw key, zone id, slab id, and slot index
- whether registry entry was absent, type mismatched, or Keg missed the slab
- if the slab is found, the current `SlotWord` generation/state/retain

Best insertion points:

- `Cap::deref`, `Cap::clone`, and `Cap::drop` on `self.slot() == None`
- or a small diagnostic helper under `registry::slot_for::<T>()` /
  `Zone::slot_from_key`

Then rerun only the focused cyclictest glibc selector that reaches
`NO_STRESS_P8`. If the typed trace names `ThreadPayload`, audit the thread
exit/group-exit/per-hart slot handoff path next. If it names a process/VM/VFS
type, follow that object family instead.

## Focused rerun on current tree

After adding typed lookup diagnostics to the panic path, the current checkout no
longer reproduced the old Cap panic in focused SMP4 `cyclictest-glibc` runs.

Implementation detail:

- `Cap::{deref,clone,downgrade,ident_ref}` now route unresolved slot lookups
  through one `expect_slot(op)` helper.
- `registry::slot_lookup_debug::<T>(key)` reports the requested type, registered
  type if any, decoded key, reason (`registry-entry-unpublished`,
  `registry-type-mismatch`, `keg-slab-or-slot-miss`, etc.), and zone
  allocated/slab/empty-slab counts.

Fresh-image witnesses from
`target/oscomp/smp4-nonltp-20260702/fix-soname-cp/cyclictest-glibc.img`:

| Log | Result |
| --- | --- |
| `target/oscomp/cyclictest-cap-diagnostic-20260705/cyclictest-glibc-typed.log` | full group END, `userspace:exited:0`, no panic/trap markers |
| `target/oscomp/cyclictest-cap-diagnostic-20260705/cyclictest-glibc-typed-r2.log` | reached `NO_STRESS_P8 end: success`; host timeout later in `STRESS_P8`; no panic/trap markers |
| `target/oscomp/cyclictest-cap-diagnostic-20260705/cyclictest-glibc-typed-r3.log` | full group END, `userspace:exited:0`, no panic/trap markers |

Targeted grep across the three logs found only `NO_STRESS_P8 end: success`,
group END, and exit markers; it found no `panic`, `panicked`, `zone Cap`,
`scause=`, or `sepc=` lines. The old failure window was therefore crossed three
times without reproducing the Cap lookup panic on the current tree.

This does not make `cyclictest-glibc` a scored pass. `tools/oscomp-judge.py`
still reports 0/4 for the complete current run because latency maxima exceed
the judge thresholds. That is a separate runtime/latency bucket from the Cap
lifetime panic classified here.

## Thread slot lifecycle cleanup

A follow-up audit found a real higher-level lifecycle gap in the thread runtime:
`enter_userspace_once()` stores both `ThreadIdentity` and
`PayloadCap<ThreadPayload>` in per-hart userspace slots so trap handoff can
resolve faults that arrive after a userspace round-trip. `run_thread()` clears
those slots on ordinary resolved waits, but `set_thread_zombie()` previously
dropped `ThreadIdentity.payload` without clearing any matching current or
userspace per-hart slots. In exit/preemption churn this can leave trap handoff
with stale owner anchors even though the semantic thread has become zombie.

The fix is intentionally key-matched rather than table-wide:

- `thread_runtime::structure::clear_thread_slots_for(thread, payload)` scans
  the per-hart current/userspace identity and payload tables.
- It clears only entries whose `SlotKey` matches the exiting
  `ThreadIdentity` or `ThreadPayload`.
- `set_thread_zombie()` calls it while the payload is still readable, after the
  existing mailbox wake-hint and before `ThreadIdentity.payload` is set to
  `None`.
- The userspace payload clear counters are updated when the helper clears a
  matching userspace payload slot.

Host regression:

- `thread_exit_clears_matching_current_and_userspace_slots` installs an exiting
  thread in current/userspace slots, installs an unrelated thread in separate
  slots, calls `step_thread_exit`, and verifies that only the matching slots
  are cleared.
- `exit_group_clears_matching_current_and_userspace_slots` installs an exiting
  process leader in current/userspace slots, calls `step_exit_group`, and
  verifies that the group-exit teardown also clears the matching per-hart
  anchors.
- Red/green check: temporarily replacing the `clear_thread_slots_for` call with
  a no-op made both slot-cleanup tests fail at the stale
  `current_thread_payload(0).is_none()` assertion; restoring the call made the
  same `current_and_userspace_slots` filter pass 2/2.

Exit-path audit:

- `step_thread_exit` calls `set_thread_zombie` directly.
- `step_exit_group` drains the process thread list and calls
  `set_thread_zombie` for each thread.
- `step_exit_group_with_signal` delegates to `step_exit_group`.
- exec sibling collapse calls `step_thread_exit` for each sibling.

Fresh-image SMP4 witness after the cleanup:

- `target/oscomp/cyclictest-cap-diagnostic-20260705/cyclictest-glibc-slotpurge.log`
  reached `NO_STRESS_P8 end: success`, `STRESS_P8 end: success`, OSComp group
  END, and `userspace:exited:0`.
- `target/oscomp/cyclictest-cap-diagnostic-20260705/cyclictest-glibc-slotpurge-r2.log`
  reached `NO_STRESS_P8 end: success`, then host-timed out in the later stress
  phase.
- `target/oscomp/cyclictest-cap-diagnostic-20260705/cyclictest-glibc-slotpurge-r3.log`
  produced no panic/trap markers before a 120s host timeout, but did not reach
  `NO_STRESS_P8 end: success`; do not count it as a strong witness for the old
  panic window.
- `target/oscomp/cyclictest-cap-diagnostic-20260705/cyclictest-glibc-slotpurge-r4.log`
  was run after a fresh `cargo xtask build --target rv64-qemu --release` and
  `make oscomp-submit-rv64`; it reached `NO_STRESS_P8 end: success`,
  `STRESS_P8 end: success`, OSComp group END, and `userspace:exited:0`.
- Targeted grep found no `panic`, `panicked`, `zone Cap`, `scause=`, or
  `sepc=` markers across the `slotpurge*.log` set.
- `tools/oscomp-judge.py` still reports 0/4 because the latency maxima exceed
  judge thresholds, which remains separate from this Cap lifetime lane.

This strengthens the lifecycle/concurrency classification and removes a real
stale-handoff risk. It still does not prove the old panic's exact `Cap<T>` type:
the original log had no typed key diagnostics, and the panic has not reproduced
since diagnostics were added.

## Verification

- `rg` over the prior log for `NO_STRESS_P8`, panic lines, shutdown summaries,
  and zone detail rows.
- `cargo xtask fault-decode --target rv64-qemu --serial
  target/oscomp/smp4-nonltp-20260702/fix-soname-cp/cyclictest-glibc.log --all
  --brief`
- Static audit of:
  - `crates/tx-substrate/src/zone/{cap,registry,keg,slab,slot,meta,mod}.rs`
  - `crates/tx-substrate/src/slot.rs`
  - `crates/tx-subsystems/src/thread_runtime/{structure,execution}.rs`
  - `crates/tx-subsystems/src/process/{structure,execution}.rs`
  - `crates/tx-kernel/src/{thread_future,trap_handoff}.rs`

Additional current-tree verification:

- `cargo check -p tx-substrate -q`
- `cargo test -p tx-substrate zone -- --nocapture`
- `cargo test -p tx-subsystems exit_group_clears_matching_current_and_userspace_slots -- --nocapture`
- red/green:
  `cargo test -p tx-subsystems current_and_userspace_slots -- --nocapture`
- `cargo test -p tx-subsystems thread_runtime -- --nocapture`
- `cargo check -p tx-subsystems -q`
- `cargo xtask build --target rv64-qemu --release`
- `make oscomp-submit-rv64 OSCOMP_SUBMIT=target/oscomp/current-submit`
- focused SMP4 QEMU runs listed above
- focused SMP4 slot-purge QEMU run:
  `target/oscomp/cyclictest-cap-diagnostic-20260705/cyclictest-glibc-slotpurge.log`
- post-rebuild focused SMP4 slot-purge QEMU run:
  `target/oscomp/cyclictest-cap-diagnostic-20260705/cyclictest-glibc-slotpurge-r4.log`
- `python3 tools/oscomp-judge.py target/oscomp/cyclictest-cap-diagnostic-20260705/cyclictest-glibc-typed-r3.log external/oscomp-autotest/kernel/judge`

Current blocker for this panic lane: none reproduced on the current tree. If the
panic returns, the new panic message should identify the failed `Cap<T>` type and
lookup reason directly.
