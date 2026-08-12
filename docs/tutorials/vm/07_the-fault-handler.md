# Chapter 7 — The fault handler

This is where everything so far converges. A user thread touches an unmapped or
under-permissioned address; the MMU traps; and the fault handler must turn the
authoritative binding (a recipe) into a materialization (a PTE) — or decide there
is no binding and signal the thread. It is the one operation that takes
`Materializer`, the one that *adds* a PTE, and the clearest place to watch the
binding/materialization split and the cross-async-wait discipline operate
together.

Linux's analog is `handle_mm_fault` → `do_fault`. txKernel's is
`fault_script_with_ufd_dispatch`.

## The shape: a re-entrant loop

The handler is an `async fn` structured as a loop with two yield points. The
shape *is* the cross-async-wait discipline from Chapter 6 made into control flow:

```rust
// vm/execution.rs:255  (simplified)
pub async fn fault_script_with_ufd_dispatch<D: UfdDispatch>(
    &self, fault: VmFault, dispatch: D,
) -> Result<PmapPublishOutcome, VmFaultError> {
    let page_range = UserRange::containing_page(fault.addr)?;
    loop {
        // 1. RESOLVE — take Materializer, observe the recipe, check permission
        let outcome = match self.try_fault_script_resolve(page_range, fault)? {
            FaultScriptResolve::Done(o) => o,
            FaultScriptResolve::Wait(token) => { await_range_lock(token).await; continue; }
        };

        // 2. (userfaultfd branch — see below)
        let ufd_reply = if let Some(tag) = outcome.entry.ufd_registration {
            dispatch_ufd_fault(&dispatch, tag.ufd_id, fault).await?
        } else { None };

        // 3. MATERIALIZE + PUBLISH — get the frame, re-observe, install the PTE
        match self.try_fault_script_materialize_and_publish(&outcome, ufd_reply)? {
            FaultScriptPublish::Done(published) => {
                self.prefault_private_anon_write_batch(&outcome);  // opportunistic
                return Ok(published);
            }
            FaultScriptPublish::Wait(token) => { await_range_lock(token).await; continue; }
        }
    }
}
```

Every `continue` is a fresh start: re-acquire the lock, re-observe the recipe,
re-attempt. A wake is a hint, never a grant (Chapter 6). The handler never holds a
reservation across the `.await`s — both yield points have already dropped their
guard before awaiting.

## Phase 1 — Resolve: observe the binding under `Materializer`

```rust
// vm/execution.rs:308  (simplified)
fn try_fault_script_resolve(&self, page_range, fault) -> Result<FaultScriptResolve, _> {
    let _guard = match self.range_lock.acquire_step(page_range, LockMode::Materializer) {
        Done(guard) => guard,
        Yield { shape, .. } => return Ok(FaultScriptResolve::Wait(token_from(shape))),
    };
    let outcome = require_fault_recipe(self, fault)?;     // ← the binding observation
    Ok(FaultScriptResolve::Done(outcome))
}
```

`require_fault_recipe` (`checks.rs:16`) is the binding observation — pure, no
mutation:

```rust
// vm/checks.rs:16
pub fn require_fault_recipe(aspace, fault) -> Result<VmFaultOutcome, VmFaultError> {
    let entry = aspace.lookup(fault.addr).ok_or(VmFaultError::NoRecipe)?;   // no binding → fault
    if !permits_fault(&entry, fault.access) {
        return Err(VmFaultError::ProtectionViolation);                     // wrong perms → fault
    }
    Ok(VmFaultOutcome { page_range, entry, access: fault.access,
                        private_identity: entry.private_identity(), .. })
}
```

Two hard exits here, both routed to **SIGSEGV** (below):

- **`NoRecipe`** — `lookup` found nothing. The address is not mapped; there is no
  authoritative binding to materialize. This is the textbook segfault.
- **`ProtectionViolation`** — a recipe exists but does not permit this access (a
  write to a read-only mapping). The binding says no.

If both checks pass, the `VmFaultOutcome` carries the observed `entry`, the
access, and — crucially — a `private_identity` snapshot used later to detect that
the recipe changed underneath us.

## Phase 3 — Materialize, then re-observe, then publish

Materialization is split from publication for one reason: **materializing may
block** (a file page may need a disk read), and a reservation must not be held
across that wait.

```rust
// vm/execution.rs:393  (simplified)
fn try_fault_script_materialize_and_publish(&self, outcome, ufd_reply)
    -> Result<FaultScriptPublish, _>
{
    // (a) materialize the frame — this is the part that may block
    let materialization = match self.materialize_step(outcome, ufd_reply) {
        VmFaultMaterializationStep::Done(m) => m,
        VmFaultMaterializationStep::Blocked(token) => {
            // reservation already dropped; await and retry the whole loop
            return Ok(FaultScriptPublish::Wait(token));
        }
        VmFaultMaterializationStep::Err(e) => return Err(e),
    };
    // (b) publish under a fresh Materializer, after re-observing
    self.try_fault_script_publish(outcome, materialization)
}
```

The materialize step dispatches on the recipe's backing:

- **`PrivateAnon`** — anonymous memory. A *read* fault installs the shared
  zero-content frame read-only; a *write* fault allocates a fresh private frame
  and records it in the mapping's `PrivatePageSet`. This is the whole of Chapter
  8.
- **`Page { offset }`** — file/tmpfs/shm/device backing. The frame comes from the
  `PageContainer`'s page cache via `materialize_page`, which may block on I/O for
  a real file. This is Chapter 9.

Then publication re-takes the lock and **re-observes the binding before
installing the PTE** — the publication rule (Chapter 2) enforced as code:

```rust
// vm/execution.rs:344  (simplified)
fn try_fault_script_publish(&self, outcome, materialization) -> Result<FaultScriptPublish, _> {
    let _guard = match self.range_lock.acquire_step(outcome.page_range, LockMode::Materializer) {
        Done(guard) => guard,
        Yield { shape, .. } => { drop(materialization); return Ok(Wait(token_from(shape))); }
    };
    require_fault_publication(self, outcome, &materialization)?;   // ← re-observe!
    let published = self.pmap.publish_page_with_replacement(
        outcome.page_range.start().containing_page(),
        materialization.page.ppn,
        materialization.publish_prot,
        materialization.page.map_pin,
        materialization.replace_existing,
    )?;
    Ok(FaultScriptPublish::Done(published))
}
```

`require_fault_publication` (`checks.rs:35`) is the anti-TOCTOU heart of the whole
design. Between the resolve in phase 1 and now, the lock may have been dropped and
re-acquired (if materialization blocked), during which a concurrent `munmap` or
`mprotect` could have changed the binding. So before publishing, it re-reads the
recipe and rejects with **`StaleRecipe`** if anything diverged:

```rust
// vm/checks.rs:35  (simplified)
pub fn require_fault_publication(aspace, outcome, materialization) -> Result<(), _> {
    let entry = aspace.recipes.lookup_view(outcome.page_range.start(), &guard)
        .ok_or(VmFaultError::StaleRecipe)?;                           // recipe vanished
    if !view_matches_entry(entry, &outcome.entry) || !entry.permits_fault(outcome.access) {
        return Err(VmFaultError::StaleRecipe);                        // recipe changed
    }
    if entry.private.map(|p| p.raw()) != outcome.private_identity {
        return Err(VmFaultError::StaleRecipe);                        // private set swapped
    }
    // backing kind + page index must still match what we materialized
    match (entry.backing, materialization.backing) { /* ...verify alignment... */ }
    Ok(())
}
```

A `StaleRecipe` is not a crash — it propagates up and the operation retries or
fails cleanly, exactly as Chapter 2's monotone-degradation argument requires. We
re-observed the truth and found it moved; we do not publish a PTE against a
binding that no longer justifies it. **The PTE is only ever installed while
holding a recipe we re-confirmed an instant before.** That is the justification
invariant, defended at the publication point.

Publication itself is the `publish_page_with_replacement` from Chapter 4 —
idempotent on an identical race winner, taking the `MapPin` that becomes the
frame's mapped-disjunct.

## Read vs write faults, and CoW

The read/write distinction lives in the materialize step. For `PrivateAnon`, a
read installs the zero frame read-only; a write allocates a private frame. The
*CoW write fault* — a write to a page currently mapped read-only because of
`fork` (Chapter 8) — arrives here as a write access whose recipe permits writing
but whose existing PTE is read-only; the materialize step recognizes it, allocates
the private copy, and publishes with `replace_existing = true` to swap the
read-only shared PTE for the writable private one. The binding always permitted
the write; the materialization was deliberately lagging, and the fault reconciles
them one page at a time.

After a successful private-anon write publish, `prefault_private_anon_write_batch`
(`execution.rs:442`) opportunistically materializes a few neighbouring pages — a
best-effort optimization for sequential write patterns (think zeroing a fresh
buffer). It uses the batch publish from Chapter 4 and silently drops any page that
races or fails; prefault is never an obligation.

## The userfaultfd branch

Between resolve and materialize sits one branch (`execution.rs:285`): if the
observed recipe carries a `ufd_registration` tag, the fault is dispatched to a
userspace fault handler (a `userfaultfd` agent) instead of being materialized by
the kernel. The agent replies with a page (`UFFDIO_COPY` → allocate a fresh frame
and copy the agent's bytes, `replace_existing = true`), a zero page (fall through
to the normal anon lane), or — out of scope for now — `CONTINUE`. A dispatcher
miss or an untagged VMA falls straight through to normal materialization, so the
hook is graceful: VMAs nobody registered behave exactly as before. This is how
txKernel supports user-space paging (live migration, `CRIU`-style restore) without
the kernel owning the page contents.

## From error to signal

The handler returns `Result<PmapPublishOutcome, VmFaultError>`. Turning a
`VmFaultError` into a thread-visible signal is the *trap layer's* job, not the
VM's — the VM reports a semantic outcome, the kernel decides policy
(`thread_future.rs`):

```
VmFaultError::NoRecipe            → SIGSEGV   (nothing mapped here)
VmFaultError::ProtectionViolation → SIGSEGV   (mapped, but not for this access)
VmFaultError::PageBeyondSize      → SIGBUS    (mapped past the file's end)
VmFaultError::WouldBlock          → retry     (transient; re-run the script)
VmFaultError::StaleRecipe         → retry     (binding moved; re-observe)
```

This is the address-as-coordinate boundary (Chapter 2, Motivation 5) closing the
loop: the VM speaks in `VmFaultError` semantics; the kernel maps that to a signal
number per its signal policy. A faulting address never becomes a raw pointer; it
becomes a recipe lookup, a materialization, and either a PTE or a signal.

## Where we go from here

You have seen the operation that consumes everything: it observes the binding,
coordinates with `Materializer`, materializes a frame (possibly blocking,
dropping the lock), re-observes to defend the invariant, and publishes a PTE — or
signals the thread. What it *materializes from* is the two-chapter remainder:
Chapter 8 is anonymous memory and the `PrivatePageSet` that makes CoW work;
Chapter 9 is page-backed memory and the page cache.

## Source anchors

- Fault loop (`fault_script_with_ufd_dispatch`): `crates/tx-subsystems/src/vm/execution.rs:255`
- Resolve phase (`try_fault_script_resolve`): same file, `:308`
- Materialize+publish (`try_fault_script_materialize_and_publish`): same file, `:393`
- Publish phase (`try_fault_script_publish`): same file, `:344`
- Prefault batch (`prefault_private_anon_write_batch`): same file, `:442`
- userfaultfd copy (`materialize_ufd_copy`): same file, `:1547`
- `require_fault_recipe` (observe binding): `crates/tx-subsystems/src/vm/checks.rs:16`
- `require_fault_publication` (re-observe, anti-TOCTOU): same file, `:35`
- `VmFaultError` variants: `crates/tx-subsystems/src/vm/structure/types.rs:1548`
- Error→signal routing (SIGSEGV/SIGBUS): `crates/tx-kernel/src/thread_future.rs:858, 946, 1088`
- Publication rule / anti-TOCTOU: `docs/design/03_memory-vm/VM_v1_2.md` §1.2, §5.1
