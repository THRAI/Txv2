# Mutation Compositions — v1

<!-- txdoc:01-SUBSTRATE-MUTATION-COMPOSITIONS-V1 -->

## Status
<!-- txdoc:MUTATION-COMPOSITIONS-STATUS-1 -->

Draft v1.

This document specifies the **substrate mutation compositions** that combine multiple substrate-primitive operations into named atomic-ish patterns. It is cited by PROCESS_v1's cross-cutting lifecycle patterns and uses the binding vocabulary from `object_model_v2.md` / `EBR_ZONE_INTERFACE_v1.md`.

The compositions specified here are:

- `structural_move` — relocate an entity from one DLL container to another, with atomic binding CAS as linearization.
- `structural_withdraw` — remove an entity from a DLL container and clear its upward binding.
- `structural_publish` — install an entity into a DLL container, setting its upward binding.

These are class-1 compositional operations: exactly one CAS is the linearization point; other mutations are materialization maintenance that may be observed in transient states but re-validate cleanly against the binding.

Companion documents:

- [`object_model_v2.md`](../00_meta-framework/object_model_v2.md) — `Binding<T>` vocabulary and binding-obligation model.
- [`EBR_ZONE_INTERFACE_v1.md`](EBR_ZONE_INTERFACE_v1.md) — current reference/evidence interface.
- [`SUBSYSTEM_ANATOMY_v2_1.md`](../00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md) §3 (five-phase discipline), §4 (substrate primitive families).
- [`01_CONCEPTS_v5.md`](../../Txv3/01_CONCEPTS_v5.md) — authoritative bindings and derived materializations.
- [`02_INVARIANTS_v5.md`](../../Txv3/02_INVARIANTS_v5.md) — ARCH-5 (publication rule).
- [`PROCESS_v1.md`](../04_process-signals/PROCESS_v1.md) — primary consumer; §8 cross-cutting patterns.

### What this document pins
<!-- txdoc:MUTATION-COMPOSITIONS-STATUS-WHAT-THIS-DOCUMENT-PINS-1 -->

- Exact signatures, contracts, and linearization points for the three compositions.
- Per-composition observer walkthroughs showing that all transient states re-validate correctly against the binding.
- Rollback behavior on failed CAS (which combinations of pre-linearization reservations drop).
- Error-propagation contracts to calling scripts.
- Relationship to object_model_v2 and the substrate zone/index/credit families.

### What this document does not pin
<!-- txdoc:MUTATION-COMPOSITIONS-STATUS-WHAT-THIS-DOCUMENT-DOES-NOT-PIN-1 -->

- The inner implementation of `DllContainer<T>` (specified in `tx-fnd/dll`).
- The inner implementation of `Index<K, V>` (specified in `tx-fnd/index`).
- Multi-authoritative (class-2) composition — explicitly out of scope per object_model_v2 §7.
- Zone reservation machinery — specified in `tx-fnd/zone`.
- Epoch guard discipline — specified in `tx-fnd/epoch`.

---

## 1. Position in the substrate
<!-- txdoc:MUTATION-COMPOSITIONS-POSITION-IN-THE-SUBSTRATE-1 -->

```
┌──────────────────────────────────────────────────────┐
│ Subsystem scripts (PROCESS_v1, THREAD_RUNTIME, ...)  │
│   invoke compositions with typed arguments           │
├──────────────────────────────────────────────────────┤
│ substrate/mutation/ (this document)                  │
│   structural_move, structural_withdraw, structural_publish
│   each composes Binding<T> + DllContainer<T> ops     │
├──────────────────────────────────────────────────────┤
│ tx-fnd/binding              tx-fnd/dll               │
│   Binding<T> with CAS        DllContainer<T> with    │
│   semantics (object_model_v2)     short-held insert/remove│
└──────────────────────────────────────────────────────┘
```

A composition's inputs are:

- A source element (`IdentRef` or `Cap` of the entity being moved/added/removed).
- A binding field on that element that names the current container.
- The DLL container(s) involved.
- Expected-old and new values for the binding CAS.

A composition's guarantees are:

- **Linearization at binding CAS.** The single CAS on the element's binding is the observable moment of the operation. Before: element is in old container and named by old binding. After: element is in new container and named by new binding.
- **Observer re-validation.** Walkers of either container re-validate elements against their binding per object_model_v2 §7. Transient presence in neither or both containers is self-correcting.
- **Atomic-or-nothing-before-CAS.** Operations before the CAS drop cleanly on failure (reservations drop; no state is published). Operations after the CAS run to completion (infallible per substrate primitives).

---

## 2. Compositions
<!-- txdoc:MUTATION-COMPOSITIONS-COMPOSITIONS-1 -->

### 2.1 structural_move
<!-- txdoc:MUTATION-COMPOSITIONS-COMPOSITIONS-STRUCTURAL-MOVE-1 -->

Relocates an element from one DLL container to another, updating its upward binding atomically.

```rust
/// Move `element` from the container named by its current binding to a new
/// container, updating the binding atomically.
///
/// The CAS on `element.binding` is the linearization point. Before: element
/// is authoritatively in `from_dll`. After: element is authoritatively in
/// `to_dll`.
pub fn structural_move<E, C>(
    element: &Cap<E>,
    binding: &Binding<C>,
    expected_old: &Cap<C>,       // must match current binding value
    new: &Cap<C>,                // the new container
    from_dll: &DllContainer<E>,  // C's members container; must currently list element
    to_dll: &DllContainer<E>,    // new C's members container
) -> Result<(), StructuralMoveErr>
where
    E: Entity,
    C: Entity,
```

**Error type:**

```rust
pub enum StructuralMoveErr {
    /// The binding did not match expected_old; element is in a different
    /// container than the caller thought. The caller may re-read and retry.
    Mismatch { actual: Option<Cap<C>> },

    /// The new DLL's insert reservation could not be acquired (e.g., list
    /// full in bounded implementations). Binding is unchanged.
    NewDllFull,
}
```

#### Implementation sketch

```rust
fn structural_move<E, C>(
    element: &Cap<E>,
    binding: &Binding<C>,
    expected_old: &Cap<C>,
    new: &Cap<C>,
    from_dll: &DllContainer<E>,
    to_dll: &DllContainer<E>,
) -> Result<(), StructuralMoveErr> {
    // Phase 1 (observe): read current binding and validate.
    // Caller is expected to have done this; the expected_old parameter
    // reflects the observation.

    // Phase 2 (reserve): reserve insertion slot in to_dll.
    //   If to_dll insert is bounded and full → return NewDllFull.
    //   This reservation drops (releases slot) if we return without committing.
    let insert_rsv = to_dll.reserve_insert(element)?;

    // Phase 3 (commit — linearization): CAS the binding.
    match binding.compare_exchange(Some(expected_old), Some(new)) {
        Err(CasError { actual, .. }) => {
            // CAS failed. insert_rsv drops; to_dll unchanged. from_dll unchanged.
            return Err(StructuralMoveErr::Mismatch { actual });
        }
        Ok(()) => {
            // Binding is now new. Element is authoritatively in to_dll per binding.
            // (Walker observing from_dll will re-validate: binding no longer
            // names from_dll; element skipped.)
        }
    }

    // Phase 4 (publish): complete DLL membership moves.
    // Both operations are infallible per DllContainer contract.
    insert_rsv.commit();                      // element now physically in to_dll
    from_dll.remove(element);                 // element no longer in from_dll

    // Note: during the window between CAS and remove, element is in BOTH DLLs
    // physically. Walkers of from_dll re-validate against the binding, find
    // the binding names to_dll, skip. Walkers of to_dll find the binding names
    // to_dll, act. Self-correcting in both cases.

    Ok(())
}
```

#### Observer correctness

Consider a walker of `from_dll` concurrent with `structural_move(element, _, _, _, from_dll, to_dll)`.

**Walker invariant** (per object_model_v2 §7):

```rust
for entry in from_dll.iter(&guard) {
    let e = entry.element(&guard);
    if e.binding.load(&guard) == Some(from_container) {
        act_on_element(e);
    } else {
        skip;  // element has moved or is moving
    }
}
```

Six transient windows the move can occupy, observed from the walker's perspective:

| Window | Binding says | from_dll contains? | to_dll contains? | Walker behavior |
|---|---|---|---|---|
| W0 (before) | from | yes | no | Acts (correct: element is in from) |
| W1 (reservation taken, CAS not done) | from | yes | reserved slot (not yet committed) | Acts (correct) |
| W2 (CAS done, insert_rsv.commit() done) | to | yes | yes | Skips (binding says to, not from) |
| W3 (from_dll.remove done) | to | no | yes | Skips if still visible via iterator; not seen if already past |
| W4 (after) | to | no | yes | N/A (walker has moved past) |

In W1, a walker acting on the element is acting on a member-of-`from` per the binding. The move has not committed yet. Correct.

In W2, the element is physically in both DLLs, but authoritatively in `to` (per binding). Walker skips per re-validation. Correct.

In W3+, the element is only in `to`. Walker of `from` doesn't see it (or skips if reached via stale iterator state).

**No window produces incorrect action.** The re-validation pattern absorbs the transient dual-membership.

Similarly for a walker of `to_dll`: in W2, element is physically present and binding says `to`. Walker acts. Before W2 (W0, W1), element is not yet in `to_dll` physically. After W2, walker continues to see element correctly.

#### Rollback on failed CAS

If the CAS fails (Mismatch):

1. `insert_rsv` drops. Its Drop impl releases the reserved insertion slot in `to_dll`. No new entry is visible there.
2. `from_dll` membership unchanged; binding unchanged.
3. Caller receives `Mismatch { actual }` and may choose to retry (re-read binding, issue fresh structural_move) or fail-up.

No observer sees any transient state different from W0.

### 2.2 structural_withdraw
<!-- txdoc:MUTATION-COMPOSITIONS-COMPOSITIONS-STRUCTURAL-WITHDRAW-1 -->

Removes an element from a DLL container and clears its upward binding.

```rust
/// Withdraw `element` from the container named by its current binding.
/// After the operation, `element.binding` is None and `element` is not
/// in any container's DLL for that role.
pub fn structural_withdraw<E, C>(
    element: &Cap<E>,
    binding: &Binding<C>,
    expected_old: &Cap<C>,       // must match current binding
    from_dll: &DllContainer<E>,  // C's members DLL; must currently list element
) -> Result<(), StructuralWithdrawErr>
where
    E: Entity,
    C: Entity,
```

**Error type:**

```rust
pub enum StructuralWithdrawErr {
    /// The binding did not match expected_old.
    Mismatch { actual: Option<Cap<C>> },
}
```

#### Implementation sketch

```rust
fn structural_withdraw<E, C>(
    element: &Cap<E>,
    binding: &Binding<C>,
    expected_old: &Cap<C>,
    from_dll: &DllContainer<E>,
) -> Result<(), StructuralWithdrawErr> {
    // Phase 1 (observe): caller has done this.

    // Phase 2 (reserve): nothing to reserve. DLL remove is infallible once
    // the element is known to be in the DLL (structurally guaranteed).

    // Phase 3 (commit — linearization): CAS binding to None.
    match binding.take_if(Some(expected_old)) {
        Err(CasError { actual, .. }) => {
            return Err(StructuralWithdrawErr::Mismatch { actual });
        }
        Ok(()) => {
            // Binding is now None. Element is authoritatively withdrawn.
        }
    }

    // Phase 4 (publish): remove from DLL. Infallible.
    from_dll.remove(element);

    // Between CAS and remove, element is physically in from_dll but binding
    // is None. Walkers re-validate: binding is None, skip.

    Ok(())
}
```

Note: `Binding::take_if(expected)` is a CAS that atomically transitions from `Some(expected)` to `None`. This is a specialization of `compare_exchange(Some(expected), None)`.

#### Observer correctness

| Window | Binding | from_dll contains? | Walker behavior |
|---|---|---|---|
| W0 (before) | Some(from) | yes | Acts (correct) |
| W1 (CAS done, remove not done) | None | yes | Skips (binding is None) |
| W2 (remove done) | None | no | N/A (walker has moved past) |

In W1, walker sees element with null binding and skips. Correct.

#### Rollback on failed CAS

Binding CAS failure means another thread concurrently modified the binding. Likely scenarios:

- Another thread already withdrew (binding is None). Caller can treat as success if idempotent, or fail.
- Another thread moved to a different container (binding is Some(other_c)). Caller's original intent might be invalid.

No reservations held, so nothing drops. Caller handles the error per its policy.

### 2.3 structural_publish
<!-- txdoc:MUTATION-COMPOSITIONS-COMPOSITIONS-STRUCTURAL-PUBLISH-1 -->

Installs an element into a DLL container and sets its upward binding.

```rust
/// Publish `element` into the container `new`, setting its upward binding.
/// Before: `element.binding` is None and element is not in any container's
/// DLL for that role. After: binding is Some(new) and element is in new's DLL.
pub fn structural_publish<E, C>(
    element: &Cap<E>,
    binding: &Binding<C>,
    new: &Cap<C>,                // the new container
    new_dll: &DllContainer<E>,   // new's members DLL
) -> Result<(), StructuralPublishErr>
where
    E: Entity,
    C: Entity,
```

**Error type:**

```rust
pub enum StructuralPublishErr {
    /// The binding was not None; element was already in some container.
    AlreadyBound { actual: Cap<C> },

    /// The new DLL's insert reservation could not be acquired.
    NewDllFull,
}
```

#### Implementation sketch

```rust
fn structural_publish<E, C>(
    element: &Cap<E>,
    binding: &Binding<C>,
    new: &Cap<C>,
    new_dll: &DllContainer<E>,
) -> Result<(), StructuralPublishErr> {
    // Phase 1 (observe): caller has done this; binding is expected None.

    // Phase 2 (reserve): insertion slot in new_dll.
    let insert_rsv = new_dll.reserve_insert(element)?;

    // Phase 3 (commit — linearization): CAS None → Some(new).
    match binding.compare_exchange(None, Some(new)) {
        Err(CasError { actual: Some(c), .. }) => {
            // Binding was not None; element is already in some container.
            // insert_rsv drops; new_dll unchanged.
            return Err(StructuralPublishErr::AlreadyBound { actual: c });
        }
        Err(_) => unreachable!("CAS compared against None; mismatch must have Some"),
        Ok(()) => {
            // Binding is now Some(new). Element is authoritatively in new.
        }
    }

    // Phase 4 (publish): complete DLL insert. Infallible.
    insert_rsv.commit();

    // Between CAS and insert commit, binding says element is in new but
    // element is not yet physically in new_dll. Walkers of new_dll don't
    // see it yet — but walkers resolving the element through other routes
    // (e.g., via its independent Cap retention) see the binding as Some(new)
    // and can query its relationship correctly.

    Ok(())
}
```

#### Observer correctness

There's an asymmetric window here: binding names new before DLL contains it. This is fine because observers who walk the DLL don't assume an element not-yet-in-DLL is absent from the role — they never observe non-existence via DLL absence alone.

| Window | Binding | new_dll contains? | Walker of new_dll behavior |
|---|---|---|---|
| W0 (before) | None | no | N/A (element not in walker's set) |
| W1 (reservation, CAS not done) | None | reserved | N/A |
| W2 (CAS done, insert not done) | Some(new) | no | Not seen (not yet in DLL) |
| W3 (insert done) | Some(new) | yes | Acts (correct) |

W2 is the asymmetric window. A walker of new_dll does not see the element yet. This is typically acceptable because:

- Reverse lookup through `element.binding` correctly reveals `Some(new)`.
- DLL walkers are usually iterating for broadcast (e.g., "signal all members") — missing a just-published element is POSIX-acceptable for such operations (the element was not yet a member during the broadcast initiation).
- For lookups targeting the specific element, callers resolve via binding, not DLL walk.

If a use case *requires* that DLL membership precede binding commit (i.e., walker must never see an element via binding before it is DLL-visible), the caller can reorder: insert first (taking reservation and committing the physical insert), then CAS binding. This changes the window to W2' = "in DLL but binding is None" which walker skips per re-validation. This alternative is slightly more bookkeeping for the caller but may be preferable for particular patterns.

Framework default: bind-then-insert as shown above. Callers wanting the alternative can do the reordering manually.

#### Rollback on failed CAS

If the CAS fails (AlreadyBound):

1. `insert_rsv` drops; new_dll unchanged.
2. Caller receives the actual binding value and handles accordingly (likely an error up to the script).

---

## 3. Compositional patterns
<!-- txdoc:MUTATION-COMPOSITIONS-COMPOSITIONAL-PATTERNS-1 -->

### 3.1 Reparenting (PROCESS_v1 §8.1)
<!-- txdoc:MUTATION-COMPOSITIONS-COMPOSITIONAL-PATTERNS-REPARENTING-PROCESS-V1-8-1-1 -->

A process P exits; its children must move from `P.children` to `init.children`, with each child's `parent` binding updating.

```rust
fn reparent_children_to_init(p: &Cap<ProcessIdentity>, init: &Cap<ProcessIdentity>) {
    let guard = epoch::guard();
    let mut iter = p.children.iter(&guard);
    while let Some(entry) = iter.next() {
        let child = entry.element(&guard).to_cap();
        loop {
            let current_parent = child.parent.load(&guard);
            if current_parent != Some(p) {
                // Already moved by someone else (shouldn't happen during p's exit).
                break;
            }
            match structural_move(
                &child,
                &child.parent,
                p,
                init,
                &p.children,
                &init.children,
            ) {
                Ok(()) => break,
                Err(StructuralMoveErr::Mismatch { .. }) => {
                    // Concurrent change; retry with fresh observation.
                    continue;
                }
                Err(StructuralMoveErr::NewDllFull) => {
                    // Shouldn't happen for init.children, which has no
                    // bounded limit. Log and panic.
                    panic!("init.children full");
                }
            }
        }
    }
}
```

Each child's move is a separate `structural_move` call. They are not cross-child atomic, but POSIX does not require cross-child atomicity for reparenting. Observers of either `p.children` or `init.children` during the window see a consistent view per `structural_move`'s correctness.

### 3.2 Session change (PROCESS_v1 §7.5.1 setsid)
<!-- txdoc:MUTATION-COMPOSITIONS-COMPOSITIONAL-PATTERNS-SESSION-CHANGE-PROCESS-V1-7-5-1-SETSID-1 -->

setsid creates a new session, a new pgroup, and moves the calling process into them.

```rust
fn step_setsid_commit(
    caller: &Cap<ProcessIdentity>,
    new_session: &Cap<Session>,
    new_pgroup: &Cap<ProcessGroup>,
    old_pgroup: &Cap<ProcessGroup>,
) {
    // First: publish new pgroup into new session.
    // (new pgroup has been created with session = None and no DLL membership.)
    structural_publish(
        &new_pgroup,
        &new_pgroup.session,
        &new_session,
        &new_session.members,
    ).unwrap_or_else(|e| unreachable!("fresh new_pgroup: {e:?}"));

    // Then: move caller from old_pgroup to new_pgroup.
    structural_move(
        &caller,
        &caller.pgrp,
        &old_pgroup,
        &new_pgroup,
        &old_pgroup.members,
        &new_pgroup.members,
    ).unwrap_or_else(|e| unreachable!("caller's pgrp observed: {e:?}"));
}
```

Two compositions in sequence. Between them, `new_pgroup` is in `new_session.members` but has no process members. An observer enumerating pgroups of the new session would see `new_pgroup` as empty — acceptable transient state.

### 3.3 setpgid (PROCESS_v1 §7.5.2)
<!-- txdoc:MUTATION-COMPOSITIONS-COMPOSITIONAL-PATTERNS-SETPGID-PROCESS-V1-7-5-2-1 -->

setpgid moves a target process from its current pgroup to another (possibly newly-created) pgroup.

```rust
fn step_setpgid_commit(
    target: &Cap<ProcessIdentity>,
    new_pgroup: &Cap<ProcessGroup>,
    current_pgroup: &Cap<ProcessGroup>,
) {
    structural_move(
        &target,
        &target.pgrp,
        &current_pgroup,
        &new_pgroup,
        &current_pgroup.members,
        &new_pgroup.members,
    )
    // Handle errors per POSIX setpgid semantics.
}
```

If the move fails with Mismatch (concurrent setpgid from another thread), retry or report error. If NewDllFull (shouldn't happen for pgroup.members), escalate.

### 3.4 Process exit — withdraw from pgroup at reap (PROCESS_v1 §8.5)
<!-- txdoc:MUTATION-COMPOSITIONS-COMPOSITIONAL-PATTERNS-PROCESS-EXIT-WITHDRAW-FROM-PGROUP-AT-REAP-PROCESS-V1-8-5-1 -->

At reap, the zombied process is withdrawn from pgrp.members and its parent's children DLL.

```rust
fn reap_proc(parent: &Cap<ProcessIdentity>, child: &Cap<ProcessIdentity>) {
    // Withdraw from parent.children. Binding: child.parent = parent.
    structural_withdraw(
        &child,
        &child.parent,
        &parent,
        &parent.children,
    ).expect("parent binding must match during reap");

    // Withdraw from pgrp.members. Binding: child.pgrp = current_pgrp.
    let guard = epoch::guard();
    let pgrp = child.pgrp.load(&guard).expect("zombie still has pgrp until reap");
    structural_withdraw(
        &child,
        &child.pgrp,
        &pgrp,
        &pgrp.members,
    ).expect("pgrp binding must match during reap");

    // ... PidNamespace.numbers withdraw, exit_status read, etc. (PROCESS_v1 §7.4.reap_child)
}
```

Each withdraw is independent. Order is chosen for observability: parent's children withdraw first (waitpid can no longer see the child in parent's children list), then pgrp withdraw.

---

## 4. Error-propagation contracts
<!-- txdoc:MUTATION-COMPOSITIONS-ERROR-PROPAGATION-CONTRACTS-1 -->

Compositions return typed errors that scripts translate to syscall errnos.

| Composition | Error | Typical syscall errno | When it happens |
|---|---|---|---|
| structural_move | Mismatch | EAGAIN (retryable), else script-specific | Concurrent binding change |
| structural_move | NewDllFull | ENOSPC or ENOMEM | DLL capacity exceeded (rare) |
| structural_withdraw | Mismatch | Script-specific | Concurrent binding change |
| structural_publish | AlreadyBound | Implementation bug — should not occur for a freshly-created element | Misuse |
| structural_publish | NewDllFull | ENOSPC or ENOMEM | DLL capacity exceeded |

Scripts wrap compositions in retry loops where appropriate (reparenting, setpgid). For non-retryable cases (fork, setsid), Mismatch indicates an implementation bug and causes panic.

---

## 5. Relationship to substrate primitives
<!-- txdoc:MUTATION-COMPOSITIONS-RELATIONSHIP-TO-SUBSTRATE-PRIMITIVES-1 -->

### 5.1 Binding<T>
<!-- txdoc:MUTATION-COMPOSITIONS-RELATIONSHIP-TO-SUBSTRATE-PRIMITIVES-BINDING-T-1 -->

The compositions use `Binding<T>`'s atomic operations:

- `compare_exchange(Some(expected), Some(new))` — used by structural_move.
- `take_if(Some(expected))` — used by structural_withdraw (or equivalently `compare_exchange(Some(expected), None)`).
- `compare_exchange(None, Some(new))` — used by structural_publish.

The binding primitive (object_model_v2 §7) provides these directly.

### 5.2 DllContainer<T>
<!-- txdoc:MUTATION-COMPOSITIONS-RELATIONSHIP-TO-SUBSTRATE-PRIMITIVES-DLLCONTAINER-T-1 -->

The compositions use DllContainer's insert/remove operations:

- `reserve_insert(element) -> Result<InsertReservation<'_>, _>` — fallible; acquires a slot.
- `InsertReservation::commit()` — infallible; publishes the insert.
- `InsertReservation::drop()` — if uncommitted, releases the slot.
- `remove(element)` — infallible; removes element from container.

The DLL substrate (in `tx-fnd/dll`) provides these.

### 5.3 Five-phase discipline
<!-- txdoc:MUTATION-COMPOSITIONS-RELATIONSHIP-TO-SUBSTRATE-PRIMITIVES-FIVE-PHASE-DISCIPLINE-1 -->

Each composition fits the five-phase discipline (SUBSYSTEM_ANATOMY §3):

| Phase | structural_move | structural_withdraw | structural_publish |
|---|---|---|---|
| 1. observe | Caller reads current binding | Caller reads current binding | Caller reads None binding |
| 2. upgrade | `expected_old: Cap<C>` held | `expected_old: Cap<C>` held | — |
| 3. reserve | `to_dll.reserve_insert` | — | `new_dll.reserve_insert` |
| 4. commit | Binding CAS (linearization) | Binding CAS (linearization) | Binding CAS (linearization) |
| 5. publish | `insert_rsv.commit`, `from_dll.remove` | `from_dll.remove` | `insert_rsv.commit` |

Failures in phases 1-3 drop the reservation cleanly. Failures at phase 4 (CAS mismatch) drop the reservation cleanly and return an error. Phase 5 is infallible.

---

## 6. Composition of compositions
<!-- txdoc:MUTATION-COMPOSITIONS-COMPOSITION-OF-COMPOSITIONS-1 -->

Scripts occasionally need to compose multiple structural ops. Examples:

- setsid: structural_publish(pgroup→session) + structural_move(proc→pgroup).
- Fork: publish child pid/tid names to `PidNamespace.numbers` (an Index, not a DLL — so `Index::commit`, not `structural_publish`) + publish child to parent.children + publish child to pgrp.members.

When such compositions are needed:

- Each individual composition is class-1 (single CAS linearization).
- The sequence of compositions is class-3 (compositional per SUBSYSTEM_ANATOMY_v2_1 §3.6): intermediate states are observable, POSIX-acceptable.
- Ordering matters for observability. Choose based on which observer needs to see which state first.

Compositions are not wrapped in an outer atomic shell. Scripts name their ordering explicitly.

---

## 7. What this document does not specify
<!-- txdoc:MUTATION-COMPOSITIONS-WHAT-THIS-DOCUMENT-DOES-NOT-SPECIFY-1 -->

- **Index-based compositions** (publish to Index<K, V>). Index has its own primitive family (`commit`, `withdraw_commit`, `swap_commit`, `install_if_match` per SUBSYSTEM_ANATOMY §4.5). These are siblings of the DLL-based structural_* compositions but don't involve DLLs. They are Index-primitive-level operations; no separate composition wrapper is needed.
- **Multi-authoritative (class-2) operations.** Rename-cross-directory and similar class-2 cases are out of scope per object_model_v2 §7. They require domain-specific atomicity (filesystem journaling, etc.).
- **Hybrid DLL+Index compositions.** E.g., "publish to DLL and to Index atomically." Currently handled by script-level ordering (publish to whichever needs to be seen first; accept the class-3 compositional window).
- **Epoch-reclamation interactions.** The structural ops work correctly under EBR per DllContainer's contract. Details of epoch advancement and reclaim-queue draining live in `tx-fnd/epoch`.
- **Performance characteristics.** Insert/remove complexity, lock contention, memory ordering. Implementation-layer.

---

## 8. Open questions
<!-- txdoc:MUTATION-COMPOSITIONS-OPEN-QUESTIONS-1 -->

- **Should structural_move support conditional moves (e.g., "move only if element's payload is Some")?** Currently no — the caller performs the check before calling, and a race would manifest as Mismatch (someone changed the binding). Worth revisiting if a common use case demands the condition at linearization time.
- **Should there be a structural_swap primitive?** "Atomically exchange element between two containers by swapping their bindings." Doesn't apply to current PROCESS_v1 use cases; can be added if needed.
- **Performance of multiple retries under Mismatch.** For high-contention scenarios (rare for process topology), a retry loop could starve. Mitigation via backoff is implementation-layer.

---

## 9. Short version
<!-- txdoc:MUTATION-COMPOSITIONS-SHORT-VERSION-1 -->

> Three substrate-mutation compositions: `structural_move` (relocate element between DLLs), `structural_withdraw` (remove element from DLL and clear binding), `structural_publish` (install element into DLL and set binding). Each is a class-1 composition with a single binding-CAS linearization; pre-CAS reservations drop cleanly on failure; post-CAS DLL operations are infallible. Observer re-validation against the binding absorbs transient dual-membership or dual-null windows per object_model_v2 §7. Callers use these for reparenting, setpgid/setsid, process exit, and similar topology mutations. Class-2 multi-authoritative atomicity is out of scope; complex topology updates are class-3 compositional per SUBSYSTEM_ANATOMY_v2_1 §3.6.
