# Global Memory-Pressure Control Plane Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> `superpowers:subagent-driven-development` (recommended) or
> `superpowers:executing-plans` to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** Provide bounded, owner-driven reclaim and allocation retry for the
4-GiB, no-swap workload without putting policy into PageAllocator, PageBacked,
ext4 or I/O manager.

**Architecture:** `tx-services` owns kernel-neutral pressure values, a pure
policy, coordinator episodes and the managed allocation gateway. Resource
owners in `tx-subsystems` and `tx-ext4` implement `ReclaimProvider`; they retain
the only authority to validate, claim, write back and evict their state.

**Tech Stack:** Rust `no_std`, `tx-services`, `tx-substrate::page_allocator`,
PageBacked, ext4 metadata caches, VFS, Zone/slab, WaitSource and StepOp.

---

## File structure

- Create `crates/tx-services/src/memory_pressure/`: values, provider contract,
  policy, coordinator and `AllocationGateway`.
- Modify `crates/tx-services/src/lib.rs` and tests: export and verify the
  control-plane service without depending on `tx-subsystems`.
- Modify `crates/tx-substrate/src/page_allocator/`: immutable pressure
  snapshots and allocation-failure facts only.
- Create `crates/tx-subsystems/src/page_backed/reclaim.rs`: PageBacked owner
  provider.
- Create `crates/tx-subsystems/src/vfs/reclaim.rs`: VFS cache provider.
- Create `crates/tx-ext4/src/reclaim.rs`: ext4 rebuildable-metadata provider.
- Modify `crates/tx-kernel/src/init/`: register providers and submit the
  coordinator task.
- Modify `crates/tx-observe-types`, schema and producer sites only after the
  ownership API is stable.

### Task 1: Define pressure values, provider protocol and pure policy seam

**Files:**
- Create: `crates/tx-services/src/memory_pressure/mod.rs`
- Create: `crates/tx-services/src/memory_pressure/model.rs`
- Create: `crates/tx-services/src/memory_pressure/provider.rs`
- Create: `crates/tx-services/src/memory_pressure/policy.rs`
- Modify: `crates/tx-services/src/lib.rs`
- Test: `crates/tx-services/src/memory_pressure/tests.rs`

- [ ] **Step 1: Add failing contract tests**

Use fake providers to prove: snapshots contain values/stable IDs but no pointer
or callback, a stale claim is a normal result, policy cannot execute work,
budgets are bounded, and feedback distinguishes binding withdrawal from actual
allocator-free progress. Target traits:

```rust
pub trait ReclaimProvider: Send + Sync + 'static {
    fn snapshot(&self, cursor: ReclaimCursor, out: &mut [ReclaimCandidate])
        -> ProviderSnapshot;
    fn try_claim(&self, request: ReclaimClaimRequest)
        -> Result<ReclaimTicket, ReclaimClaimError>;
    fn step(&self, ticket: ReclaimTicket, budget: WorkBudget) -> ReclaimStep;
    fn cancel(&self, ticket: ReclaimTicket);
}

pub trait MemoryPolicy {
    fn plan(
        &mut self,
        pressure: PressureSnapshot,
        providers: &[ProviderSnapshot],
        history: FeedbackWindow,
        out: &mut MemoryPlan,
    );
}
```

- [ ] **Step 2: Run the RED tests**

```sh
cargo test -p tx-services memory_pressure -- --nocapture
```

Expected: the module and contracts do not exist.

- [ ] **Step 3: Implement allocation-free bounded DTOs**

Use caller-provided slices/fixed-capacity rows on the critical path. Candidates
carry provider/candidate ID, generation, class, age/reference observation,
estimated free bytes, dirty/writeback/pin classification and estimated service
cost. Tickets are owned values that may cross yield; snapshots and policy plans
cannot carry owner capabilities.

- [ ] **Step 4: Verify layering**

```sh
cargo test -p tx-services memory_pressure -- --nocapture
rg -n 'tx_subsystems|tx_ext4|PageContainer|RNode|Ppn|BioVec' crates/tx-services/src/memory_pressure
```

Expected: tests pass and the dependency scan has no production match.

- [ ] **Step 5: Commit**

```sh
git add crates/tx-services
git commit -m "feat(memory): define owner-driven pressure contracts"
```

### Task 2: Add allocator snapshots and managed allocation gateway

**Files:**
- Modify: `crates/tx-substrate/src/page_allocator/mod.rs`
- Test: `crates/tx-substrate/tests/page_allocator.rs`
- Create: `crates/tx-services/src/memory_pressure/gateway.rs`
- Test: `crates/tx-services/src/memory_pressure/tests.rs`

- [ ] **Step 1: Add failing fast/slow-path tests**

Test successful fast allocation, low-water notification after success,
`NoWait/Atomic` immediate failure, `CanWait` bounded coordinator progress,
retry-after-actual-free, retry budget exhaustion, and emergency-credit use by
reclaim/writeback allocations. Instrument a fake allocator lock and assert that
provider/policy/wait hooks are never invoked while it is held.

- [ ] **Step 2: Run the RED tests**

```sh
cargo test -p tx-substrate --test page_allocator pressure_snapshot -- --nocapture
cargo test -p tx-services allocation_gateway -- --nocapture
```

- [ ] **Step 3: Export immutable allocator facts**

Add a snapshot containing total/free/reserved frames, watermark state, largest
known free order when available, allocation failure reason and monotonic progress
generation. Do not add reclaim callbacks to `PageAllocator`; snapshot methods
copy values after the allocator lock is released.

- [ ] **Step 4: Implement the gateway operation**

```rust
pub struct AllocationRequest {
    pub count: usize,
    pub order: u8,
    pub zero: ZeroPolicy,
    pub contiguous: bool,
    pub can_wait: bool,
    pub can_io: bool,
    pub urgency: AllocationUrgency,
    pub class: AllocationClass,
}

pub enum AllocationStep {
    Granted(FrameGrant),
    Progress(AllocationProgress),
    Yield { operation: AllocationOpId, wait: WaitSourceId },
    Failed(AllocationFailure),
}
```

The first attempt always uses the allocator directly. Only a low-water success
or `Exhausted` result signals the coordinator. The operation then consumes a
bounded receipt, optionally waits, retries, and stops at its scan/I/O/retry
budget. IRQ/atomic requests never enter a waitable path.

- [ ] **Step 5: Verify no upward allocator calls**

```sh
cargo test -p tx-substrate --test page_allocator -- --nocapture
cargo test -p tx-services allocation_gateway -- --nocapture
rg -n 'ReclaimProvider|MemoryPolicy|PageContainer|ext4' crates/tx-substrate/src/page_allocator
```

Expected: tests pass and the allocator scan has no upper-layer dependency.

- [ ] **Step 6: Commit**

```sh
git add crates/tx-substrate/src/page_allocator crates/tx-substrate/tests/page_allocator.rs crates/tx-services/src/memory_pressure
git commit -m "feat(memory): add managed allocation gateway"
```

### Task 3: Implement the clean PageBacked provider

**Files:**
- Create: `crates/tx-subsystems/src/page_backed/reclaim.rs`
- Modify: `crates/tx-subsystems/src/page_backed/mod.rs`
- Modify: `crates/tx-subsystems/src/lib.rs`
- Test: `crates/tx-subsystems/src/page_backed/core_tests.rs`

- [ ] **Step 1: Add failing bounded-provider tests**

Test a persistent cursor over multiple PCs, second chance via referenced bit,
fairness across owners, dirty/pinned/mapped refusal, stale generation, claim
abort, actual-free accounting and refault notification. Ensure snapshot does not
hold a PageContainer lock across policy selection and `step` holds no epoch guard
across a yield.

- [ ] **Step 2: Run the RED tests**

```sh
cargo test -p tx-subsystems --lib page_backed_reclaim_provider -- --test-threads=1
```

- [ ] **Step 3: Adapt the owner protocol from Plan A**

Candidate IDs encode only mount/object/page plus observed generation.
`try_claim` upgrades the weak PC, revalidates PageSlot state, resident binding,
no-reclaim and physical-liveness facts, then returns an owned claim. `step`
withdraws through the PageSlot protocol and reports:

```text
scanned, second_chance, claim_failed_by_reason, binding_withdrawn,
cache_pin_released, actual_frames_freed, elapsed_service_ns
```

Delete the fixed global clean sweep only after all existing callers use this
provider or a compatibility helper that delegates to it.

- [ ] **Step 4: Verify clean reclaim**

```sh
cargo test -p tx-subsystems --lib page_backed_reclaim_provider -- --test-threads=1
cargo test -p tx-subsystems --lib page_backed -- --test-threads=1
```

- [ ] **Step 5: Commit**

```sh
git add crates/tx-subsystems/src/page_backed crates/tx-subsystems/src/lib.rs
git commit -m "feat(page-backed): expose bounded clean reclaim provider"
```

### Task 4: Implement coordinator episodes and background reclaim

**Files:**
- Create: `crates/tx-services/src/memory_pressure/coordinator.rs`
- Modify: `crates/tx-services/src/memory_pressure/policy.rs`
- Modify: `crates/tx-services/src/memory_pressure/mod.rs`
- Create: `crates/tx-kernel/src/init/memory_pressure.rs`
- Modify: `crates/tx-kernel/src/init.rs`
- Test: `crates/tx-services/src/memory_pressure/tests.rs`
- Test: `crates/tx-kernel/src/init/tests.rs`

- [ ] **Step 1: Add failing state-machine tests**

Cover Normal/Low/Critical/Emergency transitions with hysteresis, one active
episode per generation, provider fairness debt, bounded per-turn work, wait-source
notification on actual progress, no-progress termination, and background task
registration exactly once.

- [ ] **Step 2: Run the RED tests**

```sh
cargo test -p tx-services memory_pressure::coordinator -- --nocapture
cargo test -p tx-kernel init::tests::memory_pressure -- --test-threads=1
```

- [ ] **Step 3: Implement first second-chance policy**

Initial ordering is: cold clean file pages, cold rebuildable VFS/ext4 metadata,
wait for already in-flight writeback, then submit a bounded dirty batch when I/O
is allowed. Anonymous pages are never eviction candidates. Policy tracks
provider scan debt and uses high/low/critical watermarks with hysteresis; it does
not see concrete PPNs or owner objects.

- [ ] **Step 4: Wire one coordinator task**

Kernel init creates the coordinator, registers the PageBacked provider and
submits one bounded StepOp/service future. Low-water events and managed allocation
failures kick it. Each turn enforces candidate, claim, elapsed-time and I/O
budgets; completion processing receives a reserved budget so lease release cannot
starve behind new submissions.

- [ ] **Step 5: Verify background/direct progress**

```sh
cargo test -p tx-services memory_pressure -- --nocapture
cargo test -p tx-kernel init::tests::memory_pressure -- --test-threads=1
cargo -q xtask unit
```

- [ ] **Step 6: Commit**

```sh
git add crates/tx-services/src/memory_pressure crates/tx-kernel/src/init.rs crates/tx-kernel/src/init/memory_pressure.rs crates/tx-kernel/src/init/tests.rs
git commit -m "feat(memory): coordinate bounded background reclaim"
```

### Task 5: Add dirty writeback, throttling and emergency credits

**Files:**
- Modify: `crates/tx-subsystems/src/page_backed/reclaim.rs`
- Modify: `crates/tx-subsystems/src/page_backed/mod.rs`
- Modify: `crates/tx-services/src/memory_pressure/model.rs`
- Modify: `crates/tx-services/src/memory_pressure/policy.rs`
- Modify: `crates/tx-services/src/memory_pressure/coordinator.rs`
- Test: `crates/tx-subsystems/src/page_backed/core_tests.rs`
- Test: `crates/tx-services/src/memory_pressure/tests.rs`

- [ ] **Step 1: Add failing dirty-pressure tests**

Test background dirty threshold, hard dirty threshold, clustered oldest-eligible
generation selection, bounded in-flight writeback, writer throttle wakeup, queue
saturation feedback, redirty, I/O error and reclaim/writeback allocations using
reserved credits under zero ordinary free frames.

- [ ] **Step 2: Run the RED tests**

```sh
cargo test -p tx-subsystems --lib pressure_writeback -- --test-threads=1
cargo test -p tx-services dirty_pressure -- --nocapture
```

- [ ] **Step 3: Add a separate dirty action**

Dirty candidates are never returned as clean reclaim claims. Policy emits a
`StartWriteback { provider, byte_budget, priority }` action. PageBacked freezes
bounded `PageDataLease` batches and submits them through the ext4/I/O-manager
data plane. Only generation-checked successful completion makes them eligible
for a later clean claim. Device queue fullness slows issuance but cannot clear
dirty state or become the threshold authority.

- [ ] **Step 4: Implement writer throttling and credits**

Writers above the hard threshold yield on coordinator/writeback progress unless
their class is no-wait. Reserve fixed bootstrap credits for claim tickets, graph
nodes, journal records, completion rows and allocator retry state. Exhausting a
credit pool returns a bounded progress/failure result; it cannot recursively
allocate through the same pressure path.

- [ ] **Step 5: Verify progress under zero free frames**

```sh
cargo test -p tx-subsystems --lib pressure_writeback -- --test-threads=1
cargo test -p tx-services dirty_pressure -- --nocapture
cargo test -p tx-ext4 --lib -- --test-threads=1
```

- [ ] **Step 6: Commit**

```sh
git add crates/tx-subsystems/src/page_backed crates/tx-services/src/memory_pressure
git commit -m "feat(memory): throttle writers through bounded writeback"
```

### Task 6: Add ext4, VFS and Zone/slab providers

**Files:**
- Create: `crates/tx-ext4/src/reclaim.rs`
- Modify: `crates/tx-ext4/src/lib.rs`
- Create: `crates/tx-subsystems/src/vfs/reclaim.rs`
- Modify: `crates/tx-subsystems/src/vfs/mod.rs`
- Modify: `crates/tx-substrate/src/zone/slab.rs`
- Modify: `crates/tx-kernel/src/init/memory_pressure.rs`
- Test: `crates/tx-ext4/src/tests_v3.rs`
- Test: `crates/tx-subsystems/src/vfs/tests.rs`
- Test: `crates/tx-substrate/src/zone/slab.rs`

- [ ] **Step 1: Add failing provider classification tests**

Assert that ext4 parsed inode/extent/bitmap/GDT cache rows and VFS positive,
negative dentry and inactive RNode cache rows may be rebuildable candidates;
frozen transactions, in-flight journal records, mounted topology roots and live
VFS entities are not. Zone may release empty slab pages or advance already-dead
retire work, but cannot select arbitrary live zone objects.

- [ ] **Step 2: Run the RED tests**

```sh
cargo test -p tx-ext4 --lib reclaim -- --test-threads=1
cargo test -p tx-subsystems --lib vfs::reclaim -- --test-threads=1
cargo test -p tx-substrate zone::slab::reclaim -- --nocapture
```

- [ ] **Step 3: Implement owner-specific providers**

Each provider uses stable owner IDs, persistent cursors, generation revalidation,
bounded tickets and actual-free feedback. Metadata caches do not join the file
page CLOCK; policy compares their snapshots/costs at the provider level. Ext4
provider eviction cannot touch `FrozenMetadataLease`, journal reservation or
checkpoint working sets.

- [ ] **Step 4: Register providers and verify layering**

```sh
cargo test -p tx-ext4 --lib reclaim -- --test-threads=1
cargo test -p tx-subsystems --lib vfs::reclaim -- --test-threads=1
cargo test -p tx-substrate zone::slab::reclaim -- --nocapture
cargo test -p tx-kernel init::tests::memory_pressure -- --test-threads=1
```

- [ ] **Step 5: Commit**

```sh
git add crates/tx-ext4/src crates/tx-subsystems/src/vfs crates/tx-substrate/src/zone crates/tx-kernel/src/init
git commit -m "feat(memory): register metadata and slab reclaim providers"
```

### Task 7: Add feedback, refault protection and migrate allocation callers

**Files:**
- Modify: `crates/tx-services/src/memory_pressure/`
- Modify: `crates/tx-subsystems/src/page_backed/reclaim.rs`
- Modify: `crates/tx-subsystems/src/page_backed/mod.rs`
- Modify: `crates/tx-subsystems/src/vm/` allocation callsites
- Modify: `crates/tx-kernel/src/init/memory_pressure.rs`
- Modify: `docs/progress/plans/2026-07-25-memory-io-ext4-repair.json`
- Modify: `docs/progress/STATUS.md`

- [ ] **Step 1: Add failing feedback and callsite tests**

Test avoidable refault distance, provider scan efficiency, fairness debt, policy
protection after rapid refault, and every managed allocation class. Static scan
must enumerate direct `reserve_frame` callers and require each to be classified
as managed, boot-only, atomic, DMA, page-table or kernel-critical.

- [ ] **Step 2: Run the RED tests and inventory**

```sh
cargo test -p tx-services refault -- --nocapture
rg -n 'page_allocator::reserve_(frame|contiguous)' crates/tx-subsystems crates/tx-kernel crates/tx-ext4
```

- [ ] **Step 3: Implement bounded ghost/refault history**

Record compact `(provider, object, offset/class, eviction_generation)` ghosts,
not retained pages. On refault, report distance and service cost; policy protects
the working set and shifts scan budgets away from ineffective providers. Keep
the first policy deterministic and bounded.

- [ ] **Step 4: Migrate managed callers**

Replace the PageBacked-private `reserve_frame_with_reclaim` helper with
`AllocationGateway`. Migrate file-cache fill and waitable VM allocation first.
Keep boot, pmap atomic and explicit critical-reserve paths direct and documented.
No caller may synchronously recurse into filesystem I/O from allocator state.

- [ ] **Step 5: Run the control-plane gate**

```sh
cargo test -p tx-services memory_pressure -- --nocapture
cargo test -p tx-subsystems --lib page_backed -- --test-threads=1
cargo test -p tx-kernel vm -- --test-threads=1
cargo xtask lint invariants memory-io-ownership
cargo -q xtask unit
cargo xtask progress validate
git diff --check
```

- [ ] **Step 6: Commit**

```sh
git add crates/tx-services crates/tx-subsystems crates/tx-kernel docs/progress
git commit -m "feat(memory): close managed allocation and refault feedback loop"
```
