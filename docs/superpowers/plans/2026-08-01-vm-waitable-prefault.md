# VM Waitable Prefault Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> `superpowers:test-driven-development`, `tx-vm-pagebacked`,
> `tx-step-migration`, and `tx-xtask`. Execute task-by-task with
> `superpowers:subagent-driven-development` or
> `superpowers:executing-plans`.

**Goal:** turn eager reserve/prefault into a waitable `StepOp`, preserve exact
RangeLock and file-fetch waits through syscall/fault drivers, and prove cold
file-backed VMA completion.

**Architecture:** `ReserveUserRangeOp` owns a page cursor plus an optional owned
`VmFaultOutcome` and returns typed `PageProgress`. VM observes recipes and
publishes PTEs only under short-lived `Materializer` reservations, while
PageContainer owns file fetch and residency. A monotonic recipe publication
sequence makes the common resume path one word comparison; sequence mismatch
falls back to full target-entry validation, and only an incompatible target
recipe restarts resolution.

**Tech Stack:** Rust `no_std`, tx-substrate step/wake primitives,
tx-subsystems VM/PageBacked, tx-scripts central driver, tx-shims Linux syscall
dispatch, host unit tests, xtask invariant/progress gates.

---

## File Map

- `crates/tx-subsystems/src/vm/user_access.rs`: one-page prefault state
  transition and error/wait mapping.
- `crates/tx-subsystems/src/vm/step_ops.rs`: stateful range cursor and
  `PageProgress` `StepOp` implementation.
- `crates/tx-subsystems/src/vm/structure/recipe.rs`: monotonic recipe
  publication sequence and stable stamped lookup.
- `crates/tx-subsystems/src/vm/structure/types.rs`: generation stamp carried by
  the owned `VmFaultOutcome`.
- `crates/tx-subsystems/src/vm/checks.rs`: generation fast path, field-level
  slow path, and local materialization checks.
- `crates/tx-subsystems/src/vm/adapter.rs`: exact `WaitSourceId` lookup behind
  the VM adapter boundary.
- `crates/tx-subsystems/src/vm/execution.rs`: demand-fault exact-token wait and
  retry.
- `crates/tx-subsystems/src/page_backed/core_tests.rs`: reusable stateful file
  wait fixture plus cold file-backed reserve/fault integration tests.
- `crates/tx-shims/src/linux_syscall/io.rs`: async prefault drivers and
  hot-only writev behavior.
- `crates/tx-shims/src/linux_syscall/mod.rs`: synchronous writev fallthrough.
- `crates/tx-shims/src/linux_syscall/tests/fd_ops_wave3.rs`: runtime writev
  fallback tests.
- `xtask/src/lint_invariants_syscall.rs`: reverse one-shot ratchets to require
  full drivers/fallback.
- `docs/design/03_memory-vm/VM_v1_2.md`: generation-based revalidation contract.
- `docs/progress/research/2026-08-01-vm-copy-zero-copy-boundary.md`: durable
  mmap/O_DIRECT/buffered-copy boundary.
- `docs/progress/STATUS.md` and
  `docs/progress/plans/2026-08-01-vm-waitable-prefault.json`: completion
  catch-up.

## Task 1: Pin Cold File-Backed Reserve Behavior

**Files:**

- Modify: `crates/tx-subsystems/src/page_backed/core_tests.rs`

- [ ] **Step 1: Replace the fixed-token fixture with a controllable wait fixture**

Keep the existing `FsOps` stub body, but give `BlockingFs` real state:

```rust
struct BlockingFs {
    ready: AtomicBool,
    fetches: AtomicUsize,
    source_id: u64,
    source: Arc<crate::page_backed::adapter::wait_routing::WaitSource>,
}

impl BlockingFs {
    fn new() -> Self {
        let source_id = crate::allocate_notification_source_id();
        let source = crate::page_backed::adapter::wait_routing::new_wait_source(source_id);
        Self {
            ready: AtomicBool::new(false),
            fetches: AtomicUsize::new(0),
            source_id,
            source,
        }
    }

    fn complete(&self) {
        self.ready.store(true, Ordering::Release);
        self.source.notify(step_engine::InterestMask::new(0x44));
    }
}
```

Update `fetch_page` so it increments `fetches`, returns the fixture's exact
wait source while cold, and returns the zero frame when ready. Add `Drop` to
unregister the source. Update existing `Arc::new(BlockingFs)` construction
sites to `Arc::new(BlockingFs::new())` and preserve their token assertions by
reading `fs.source_id`.

- [ ] **Step 2: Add the failing cold shared-VMA reserve test**

Map a one-page `PageContainerKind::File` as shared/readable, construct
`ReserveUserRangeOp`, and assert its first step yields the fixture source:

```rust
let mut op = ReserveUserRangeOp::new(&aspace, range, UserAccessKind::Read);
let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
assert_eq!(
    op.step(&mut ctx),
    V3Out::yield_on_wait_source(PageProgress::EMPTY, fs.source_id, 0x44),
);
assert_eq!(pc.resident_pages(), 0);
assert!(aspace.pmap().lookup(range.start().containing_page()).is_none());
```

- [ ] **Step 3: Run the test and verify RED**

Run:

```bash
cargo test -p tx-subsystems page_container_reserve_user_range_yields_for_cold_file_vma -- --test-threads=1
```

Expected: FAIL because `ReserveUserRangeOp::new` does not exist and the current
one-shot reserve folds the file wait to `EFAULT`.

- [ ] **Step 4: Add failing retry, two-page, RangeLock, and terminal-error tests**

Add focused tests proving:

```rust
// Cold -> ready retry.
fs.complete();
assert_eq!(op.step(&mut ctx), V3Out::Continue { progress: PageProgress::new(1) });
assert_eq!(op.step(&mut ctx), V3Out::Done(()));

// Writer conflict: exact AddressSpace RangeLock release source.
assert_eq!(
    blocked.step(&mut ctx),
    V3Out::yield_on_wait_source(
        PageProgress::EMPTY,
        aspace.range_lock().wait_source_id(),
        RANGE_LOCK_RELEASE_MASK,
    ),
);
```

The two-page case seeds page 0, leaves page 1 cold, and checks that retry does
not increment page-0 fetch/publication counters. Reuse the existing unmapped and
protection-mismatch expectations to keep `EFAULT` terminal.

- [ ] **Step 5: Run the focused group and verify RED for the intended reasons**

```bash
cargo test -p tx-subsystems page_container_reserve_user_range -- --test-threads=1
```

Expected: FAIL on one-shot/error-folding behavior, not fixture construction.

## Task 2: Add Recipe-Generation Revalidation

**Files:**

- Modify: `crates/tx-subsystems/src/vm/structure/recipe.rs`
- Modify: `crates/tx-subsystems/src/vm/structure/types.rs`
- Modify: `crates/tx-subsystems/src/vm/checks.rs`
- Modify: `crates/tx-subsystems/src/vm/tests.rs`
- Modify: `docs/design/03_memory-vm/VM_v1_2.md`

- [ ] **Step 1: Add failing fast, slow, stale, and ABA tests**

Construct a file-backed outcome, then exercise four publication histories:

```rust
let outcome = require_fault_recipe(&aspace, fault).unwrap();
let materialization = test_pagebacked_materialization(&outcome);

// No recipe publication: one generation comparison, no tree slow path.
let slow_before = debug_fault_full_revalidations();
require_fault_publication(&aspace, &outcome, &materialization).unwrap();
assert_eq!(debug_fault_full_revalidations(), slow_before);

// Unrelated VMA publication: generation differs, fields still match.
aspace.map(unrelated_entry, MapPlacement::RequireFree).unwrap();
require_fault_publication(&aspace, &outcome, &materialization).unwrap();
assert_eq!(debug_fault_full_revalidations(), slow_before + 1);

// Target publication: slow path rejects stale authority.
aspace.protect(outcome.page_range, Prot::READ).unwrap();
assert_eq!(
    require_fault_publication(&aspace, &outcome, &materialization),
    Err(VmFaultError::StaleRecipe),
);
```

The ABA case rewrites the target twice so its final visible fields match the
original and asserts that the captured generation still differs. This test is
about detecting publication history; field equality may still let the slow
path continue safely.

- [ ] **Step 2: Run the tests and verify RED**

```bash
cargo test -p tx-subsystems vm_checks_require_fault_publication -- --test-threads=1
```

Expected: FAIL because `VmFaultOutcome` has no generation stamp and every
publication revalidation currently traverses the recipe tree.

- [ ] **Step 3: Add a stable sequence to `RecipeIndex`**

Use a full `AtomicU64`, where even values are stable publications and odd
values mean a prepared writer is committing:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::vm) struct RecipeGeneration(u64);

pub(in crate::vm) struct RecipeIndex {
    current: Published<RecipeTree>,
    mutation: VmSpinMutex<()>,
    sequence: AtomicU64,
    // existing test fields remain unchanged
}
```

Initialize new and cloned address spaces at stable generation zero. Add a
bounded stamped lookup: two matching even acquire loads return a stable stamp;
an interleaving writer returns the observed owned entry without a stamp so
publication must use the slow path. Do not spin inside the step:

```rust
pub(in crate::vm) fn lookup_stamped(
    &self,
    addr: UserVirtAddr,
    guard: &Guard<'_>,
) -> Option<(VmEntry, Option<RecipeGeneration>)> {
    let before = self.sequence.load(Ordering::Acquire);
    let entry = self.pinned(guard).lookup(addr)?;
    let after = self.sequence.load(Ordering::Acquire);
    let generation = (before == after && after & 1 == 0)
        .then_some(RecipeGeneration(after));
    Some((entry, generation))
}
```

Prepare the replacement before changing the sequence. While holding the
existing mutation lock, publish odd, commit the infallible prepared root swap,
then release-publish the next even value. Use checked addition and fail before
the odd store on exhaustion so sequence wrap can never create ABA:

```rust
let stable = self.sequence.load(Ordering::Relaxed);
assert_eq!(stable & 1, 0, "recipe writer entered with odd sequence");
let publishing = stable.checked_add(1).expect("recipe sequence exhausted");
let next = stable.checked_add(2).expect("recipe sequence exhausted");
self.sequence.store(publishing, Ordering::Release);
replacement.commit();
self.sequence.store(next, Ordering::Release);
```

- [ ] **Step 4: Split publication validation into fast and slow paths**

Store `Option<RecipeGeneration>` in `VmFaultOutcome`. Make
`require_fault_recipe` use `lookup_stamped` under one guard. Refactor
`require_fault_publication` so local materialization validation always runs,
while tree lookup runs unless a stable generation matches:

```rust
if !outcome.recipe_generation.is_some_and(|captured| {
    aspace.recipes.stable_generation() == Some(captured)
}) {
    require_current_recipe_matches(aspace, outcome)?;
    debug_count_fault_full_revalidation();
}
require_materialization_matches_outcome(outcome, materialization)
```

The local check still compares backing kind, special backing, and page index;
generation equality proves recipe stability, not that an arbitrary
`VmFaultMaterialization` is valid. Do not use the current root pointer as the
stamp because EBR reclamation and allocation reuse permit pointer ABA.

- [ ] **Step 5: Document and verify the contract**

Update `VM_v1_2.md` so fresh post-yield validation permits the generation fast
path and requires the field-level fallback on mismatch. Run:

```bash
cargo test -p tx-subsystems vm_checks_require_fault_publication -- --test-threads=1
cargo test -p tx-subsystems vm_recipe -- --test-threads=1
```

Expected: PASS; unchanged publication uses the fast path, unrelated mutation
uses and passes the slow path, target mutation is stale, and two rewrites never
produce a false generation match.

## Task 3: Implement The Waitable Reserve StepOp

**Files:**

- Modify: `crates/tx-subsystems/src/vm/user_access.rs`
- Modify: `crates/tx-subsystems/src/vm/step_ops.rs`
- Test: `crates/tx-subsystems/src/page_backed/core_tests.rs`

- [ ] **Step 1: Extract a one-page VM transition**

Change `publish_fault_materialization` to borrow `&VmFaultOutcome`; publication
consumes the materialization/`MapPin`, but it does not need to consume the owned
recipe snapshot. Replace the synchronous whole-range helper with a one-page
step helper that retains that snapshot across file and RangeLock waits:

```rust
pub(crate) fn reserve_user_page_for_access_step(
    &self,
    page: UserPage,
    kind: UserAccessKind,
    pending: &mut Option<VmFaultOutcome>,
) -> StepOutcome<(), NoProgress> {
    if self
        .pmap
        .lookup(page)
        .is_some_and(|snapshot| snapshot.prot.permits(kind.required_prot()))
    {
        *pending = None;
        return StepOutcome::done(());
    }

    if pending.is_none() {
        let page_addr = match page.checked_start_addr() {
            Ok(addr) => addr,
            Err(_) => return StepOutcome::err(Errno::EFAULT.into()),
        };
        let fault = VmFault::new(page_addr, kind.required_prot());
        *pending = match self.resolve_fault(fault) {
            Ok(outcome) => Some(outcome),
            Err(VmFaultError::WouldBlock) => {
                return crate::vm::notification::range_lock_blocked(
                    self.range_lock().release_endpoint(),
                );
            }
            Err(_) => return StepOutcome::err(Errno::EFAULT.into()),
        };
    }

    let outcome = pending.as_ref().expect("pending fault outcome");
    let guard = step_engine::guard();
    let materialization = match outcome.materialize_pagebacked_step(&guard) {
        VmFaultMaterializationStep::Done(value) => value,
        VmFaultMaterializationStep::Blocked(token) => {
            drop(guard);
            return notification::yield_wait_token(NoProgress, token);
        }
        VmFaultMaterializationStep::Err(_) => {
            *pending = None;
            return StepOutcome::err(Errno::EFAULT.into());
        }
    };
    drop(guard);
    match self.publish_fault_materialization(outcome, materialization) {
        Ok(_) => {
            *pending = None;
            StepOutcome::done(())
        }
        Err(VmFaultError::WouldBlock) => crate::vm::notification::range_lock_blocked(
            self.range_lock().release_endpoint(),
        ),
        Err(VmFaultError::StaleRecipe) => {
            *pending = None;
            StepOutcome::Continue { progress: NoProgress }
        }
        Err(_) => {
            *pending = None;
            StepOutcome::err(Errno::EFAULT.into())
        }
    }
}
```

Use explicit matches rather than `?` so every terminal error, stale retry, and
exact wait-source conversion remains visible in the `StepOutcome` control flow.

- [ ] **Step 2: Make `ReserveUserRangeOp` stateful**

```rust
pub struct ReserveUserRangeOp<'a> {
    aspace: &'a AddressSpace,
    kind: UserAccessKind,
    pages: UserPageIter,
    pending: Option<VmFaultOutcome>,
}

impl<'a> ReserveUserRangeOp<'a> {
    pub fn new(aspace: &'a AddressSpace, range: UserRange, kind: UserAccessKind) -> Self {
        Self { aspace, kind, pages: range.iter_pages(), pending: None }
    }
}

impl<I: SubjectIdentity> StepOp<I> for ReserveUserRangeOp<'_> {
    type Output = ();
    type Progress = PageProgress;

    fn step(&mut self, ctx: &mut ScriptCtx<I>) -> StepOutcome<(), PageProgress> {
        let _ = ctx.subject();
        let mut next = self.pages;
        let Some(page) = next.next() else { return StepOutcome::done(()) };
        match self.aspace.reserve_user_page_for_access_step(
            page,
            self.kind,
            &mut self.pending,
        ) {
            StepOutcome::Done(()) => {
                self.pages = next;
                StepOutcome::continue_with(PageProgress::new(1))
            }
            StepOutcome::Yield { shape, .. } => StepOutcome::Yield {
                progress: PageProgress::EMPTY,
                shape,
            },
            StepOutcome::Err(errno) => StepOutcome::Err(errno),
            StepOutcome::Continue { .. } => StepOutcome::Continue {
                progress: PageProgress::EMPTY,
            },
        }
    }
}
```

Remove both `OneShotStepOp` implementations and their import. The iterator is
committed only after the page succeeds, so Yield retries the same page with its
owned stamped outcome. A stale target recipe clears `pending` and returns an
empty `Continue`, causing fresh resolution without advancing the cursor.

- [ ] **Step 3: Run the focused tests and verify GREEN**

```bash
cargo test -p tx-subsystems page_container_reserve_user_range -- --test-threads=1
cargo test -p tx-subsystems vm_reserve_user_range -- --test-threads=1
```

Expected: PASS; cold file and RangeLock cases yield exact sources, retry reaches
Done, and terminal error tests remain unchanged.

- [ ] **Step 4: Refactor comments and names**

Remove claims that reserve is synchronous/one-shot. Document that cursor plus
owned `VmFaultOutcome` are the only cross-yield state and that no guard,
witness, RangeLock reservation, materialization, or `MapPin` crosses Yield.

- [ ] **Step 5: Re-run focused tests**

Run the two Task 3 commands again; expected PASS.

## Task 4: Migrate Syscall Prefault Drivers

**Files:**

- Modify: `crates/tx-shims/src/linux_syscall/io.rs`
- Modify: `xtask/src/lint_invariants_syscall.rs`

- [ ] **Step 1: Reverse the xtask expectations first**

Change the three async-prefault tests to require `drive(` and forbid
`drive_oneshot`. Change the writev test to require a hot-PTE gate and async
fallthrough rather than a reserve one-shot.

- [ ] **Step 2: Run the xtask test and verify RED**

```bash
cargo test -p xtask lint_invariants_syscall -- --test-threads=1
```

Expected: FAIL because the three async paths still call `drive_oneshot` and the
writev helper still constructs `ReserveUserRangeOp`.

- [ ] **Step 3: Add one local async prefault helper**

Avoid duplicating the six-argument drive setup:

```rust
async fn drive_user_prefault(
    ctx: &SyscallCtx<'_>,
    range: UserRange,
    kind: UserAccessKind,
) -> Result<(), tx_subsystems::execution::Errno> {
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mailbox = script_ctx.mailbox().cloned();
    let delegates = script_ctx.delegate_registry().cloned();
    let timers = script_ctx.timer_registrar().cloned();
    tx_scripts::drive::drive(
        ReserveUserRangeOp::new(&ctx.aspace, range, kind),
        &mut script_ctx,
        DriveMode::Waiting,
        mailbox.as_ref(),
        delegates.as_deref(),
        timers.as_ref(),
    )
    .await
}
```

Use it from buffered write (`Read` access), direct write/read (directional
access), and buffered read (`Write` access). Keep file-operation drive mode

- [ ] **Step 4: Run focused shim/xtask tests and verify GREEN**

```bash
cargo test -p xtask lint_invariants_syscall -- --test-threads=1
cargo test -p tx-shims pagebacked -- --test-threads=1
```

Expected: PASS.

## Task 5: Preserve The Synchronous writev Hot Lane

**Files:**

- Modify: `crates/tx-shims/src/linux_syscall/io.rs`
- Modify: `crates/tx-shims/src/linux_syscall/mod.rs`
- Test: `crates/tx-shims/src/linux_syscall/tests/fd_ops_wave3.rs`

- [ ] **Step 1: Add failing fallback tests**

Cover two cases:

1. cold/unpublished iovec metadata or payload makes
   `dispatch_writev_pagebacked_oneshot` return `None`;
2. a PageBacked destination Yield before byte progress also returns `None` and
   the full dispatcher completes through async `sys_writev` after wake.

Keep the existing socketpair rejection test.

- [ ] **Step 2: Run and verify RED**

```bash
cargo test -p tx-shims writev_pagebacked -- --test-threads=1
```

Expected: FAIL because the current wrapper converts helper `None` into
`Some(Error(EAGAIN))`.

- [ ] **Step 3: Add a non-materializing hot-PTE query**

Add a VM helper that checks every page with `pmap.lookup` and protection only;
it must never resolve a recipe or fetch a page:

```rust
pub fn user_range_is_ready_for_access(
    &self,
    range: UserRange,
    kind: UserAccessKind,
) -> bool {
    range.iter_pages().all(|page| {
        self.pmap
            .lookup(page)
            .is_some_and(|pte| pte.prot.permits(kind.required_prot()))
    })
}
```

Use it before bootstrap-copying the iovec array and before each payload. Remove
`ReserveUserRangeOp` from the synchronous helper.

- [ ] **Step 4: Propagate `None` to the full dispatcher**

Replace the EAGAIN conversion with fallthrough:

```rust
let result = sys_writev_pagebacked_oneshot(req.args, ctx)?;
// emit traces only for a completed synchronous result
Some(result)
```

If the helper has already committed bytes, keep returning the partial byte
count rather than `None`.

- [ ] **Step 5: Run and verify GREEN**

```bash
cargo test -p tx-shims writev_pagebacked -- --test-threads=1
cargo test -p xtask lint_invariants_syscall -- --test-threads=1
```

Expected: PASS; cold paths fall through, hot paths remain synchronous, and no
reserve one-shot remains.

## Task 6: Fix Demand-Fault Exact Wait Routing And Continue

**Files:**

- Modify: `crates/tx-subsystems/src/vm/adapter.rs`
- Modify: `crates/tx-subsystems/src/vm/execution.rs`
- Test: `crates/tx-subsystems/src/page_backed/core_tests.rs`

- [ ] **Step 1: Add failing exact-source and continuation tests**

Poll `aspace.fault_script(read_fault)` once and assert `Pending`. Fire the
RangeLock source alone and assert it remains pending. Then call
`BlockingFs::complete()`, poll again, and assert the future completes with a
published PTE. Record `debug_fault_full_revalidations()` before the first poll
and assert it is unchanged after ordinary file completion, proving that resume
continued with the stamped outcome rather than resolving the tree again.

Add two variants while the file is pending:

```rust
// Unrelated rewrite: slow revalidation accepts the unchanged target.
aspace.map(unrelated_entry, MapPlacement::RequireFree).unwrap();
fs.complete();
assert!(poll_to_ready(&mut fault_future).unwrap().is_published());

// Target rewrite: slow revalidation rejects, then outer resolution sees the
// new protection and terminates instead of publishing the stale frame.
aspace.protect(target_range, Prot::NONE).unwrap();
fs.complete();
assert_eq!(
    poll_to_ready(&mut stale_future),
    Err(VmFaultError::ProtectionViolation),
);
```

- [ ] **Step 2: Run and verify RED**

```bash
cargo test -p tx-subsystems fault_script_cold_file_waits_on_file_source -- --test-threads=1
```

Expected: FAIL because the current script waits on RangeLock even though the
returned token names the file source.

- [ ] **Step 3: Expose exact source lookup through the VM adapter**

```rust
pub fn lookup_wait_source(source_id: u64) -> Option<Arc<WaitSource>> {
    tx_substrate::wake::lookup_source(WaitSourceId::new(source_id))
}
```

- [ ] **Step 4: Route the exact token and preserve the stamped outcome**

Add an async helper in `vm/execution.rs`:

```rust
async fn await_fault_wait_token(
    range_endpoint: &impl WaitEndpoint,
    token: WaitToken,
) -> Result<(), VmFaultError> {
    if range_endpoint.source_id().raw() == token.source_id() {
        let _ = crate::wait_source::wait_on_endpoint(range_endpoint, token.interest()).await;
        return Ok(());
    }
    if let Some(wait) =
        crate::wait_source::wait_on_registered_source_id(token.source_id(), token.interest())
    {
        let _ = wait.await;
        return Ok(());
    }
    if let Some(source) = vm::adapter::wait_routing::lookup_wait_source(token.source_id()) {
        let _ = crate::wait_source::wait_on_endpoint(&source, token.interest()).await;
        return Ok(());
    }
    Err(VmFaultError::WouldBlock)
}
```

Call it from both resolve-side and materialize/publish-side `Wait` branches.
Reshape `fault_script` as an outer recipe-resolution loop plus an inner
materialize/publish loop:

```rust
'resolve: loop {
    let outcome = await_resolve_fault(fault).await?;
    loop {
        let materialization = match try_materialize(&outcome)? {
            Done(materialization) => materialization,
            Wait(token) => {
                await_fault_wait_token(&range_endpoint, token).await?;
                continue;
            }
        };
        match try_fault_script_publish(&outcome, materialization)? {
            Done(published) => return Ok(published),
            Wait(token) => {
                await_fault_wait_token(&range_endpoint, token).await?;
                continue;
            }
            StaleRecipe => continue 'resolve,
        }
    }
}
```

The owned outcome may cross the await; its guard and `Materializer` reservation
may not. Publication reacquires `Materializer`, uses the generation fast path,
and invokes the full field slow path only on mismatch. A publication-side wait
drops the materialization/`MapPin` before awaiting and rematerializes from the
same outcome after wake.

- [ ] **Step 5: Run and verify GREEN**

```bash
cargo test -p tx-subsystems fault_script_cold_file_waits_on_file_source -- --test-threads=1
cargo test -p tx-subsystems fault_script_recipe_generation -- --test-threads=1
cargo test -p tx-subsystems fault_script_yields_on_writer_conflict_and_completes_after_release -- --test-threads=1
```

Expected: PASS for file and RangeLock sources; unchanged recipe resumes without
a tree lookup, unrelated rewrite takes the accepting slow path, and target
rewrite restarts outer resolution.

## Task 7: Full Verification And Progress Catch-Up

**Files:**

- Create: `docs/progress/research/2026-08-01-vm-copy-zero-copy-boundary.md`
- Modify: `docs/progress/STATUS.md`
- Modify: `docs/progress/plans/2026-08-01-vm-waitable-prefault.json`

- [ ] **Step 1: Run scoped formatting and tests**

```bash
cargo fmt --check
cargo test -p tx-subsystems vm_checks_require_fault_publication -- --test-threads=1
cargo test -p tx-subsystems fault_script_recipe_generation -- --test-threads=1
cargo test -p tx-subsystems vm -- --test-threads=1
cargo test -p tx-subsystems page_backed -- --test-threads=1
cargo test -p tx-shims writev_pagebacked -- --test-threads=1
cargo test -p tx-kernel thread_future -- --test-threads=1
cargo test -p tx-scripts -- --test-threads=1
```

Expected: PASS, or record any inherited unrelated failure with its exact test
name and error before proceeding.

- [ ] **Step 2: Run architecture gates**

```bash
cargo xtask lint invariants step
cargo xtask lint invariants no-adhoc-drive
cargo xtask lint docs
git diff --check
```

Expected: PASS.

- [ ] **Step 3: Record the zero-copy boundary**

The research note must state:

- file-backed `mmap` maps the PC frame directly;
- O_DIRECT pins user frames and submits them as `BioVec` without PC memcpy;
- ordinary buffered I/O retains one user-frame/PC-frame memcpy;
- replacing arbitrary user PTEs would change VMA/COW/buffer semantics;
- same-PPN overlap handling is a separate correctness follow-up.

The completion catch-up must also record that ordinary file completion keeps
the owned fault outcome and uses the generation fast path; only stale target
authority restarts recipe resolution.

- [ ] **Step 4: Close progress state**

Update the JSON plan steps and verification entries with actual results, set
status to `complete` only if all required work is done, and replace the proposed
STATUS entry with changed surface, verification, next step, and blockers.

- [ ] **Step 5: Validate progress records**

```bash
cargo xtask progress validate
cargo xtask lint docs
git diff --check
```

Expected: PASS.
