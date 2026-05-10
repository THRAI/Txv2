# Migration: Blast Radius and Landing Order — v2 (post-merge)

<!-- txdoc:TXV3-BLAST-RADIUS-V2 -->

**Status.** v2 (Txv3 refresh, 2026-05; supersedes v1 with post-merge numbers).
**Purpose.** Quantify the cost of landing the v3 architectural changes against the merged tree. Recommend a landing order updated for the absence of worktree-rebase coordination.
**Audience.** Implementation lead; reviewers of the migration ADR.
**Method.** Survey of merged mainline as of 2026-05 (the funny-hugle / objective-davinci / distracted-lichterman worktrees have landed). Grep-based call-site counts; per-change blast estimate.
**Diff vs v1.** v1 modeled the world before the layered-subsystems merge; v2 reflects the merged tree. The refactor surface is now entirely on mainline; the previously-skeleton crates are populated; tests are part of the surface.

---

## 1. Code volume baseline (post-merge)

<!-- txdoc:BLAST-V2-BASELINE-1 -->

```
mainline (excluding target/, ext4 backend, HAL boards):
  tx-subsystems        33,979 lines  104 files   ← was 9 lines (skeleton)
  tx-shims             15,266 lines   29 files   ← was 4 lines (skeleton)
  tx-substrate         13,061 lines   51 files   ← was 12,217
  boards/tx-hal-rv64    8,546 lines   18 files   ← was 6,265 (HAL grew)
  tx-reactor            8,427 lines   31 files   ← unchanged
  tx-kernel             6,570 lines   16 files   ← was 8,792 (some moved into subsystems)
  tx-scripts            3,786 lines    8 files   ← was 8 lines (skeleton)
  tx-fs                 2,794 lines    6 files   ← was 8 lines (skeleton)
  tx-ext4-format        2,706 lines    5 files   ← unchanged
  boards/tx-hal-m1dock  2,252 lines    3 files   ← unchanged
  tx-hal                1,894 lines    4 files   ← was 1,387
  boards/tx-hal-la64    1,352 lines    1 files   ← was 534
  tx-ext4               1,318 lines    7 files   ← was 849
  tx-drivers              196 lines    1 files   ← unchanged
  tx-policy                 9 lines    1 files   ← still skeleton
  tx-services               7 lines    1 files   ← still skeleton
```

Total kernel-material LoC: ~88,000 (was ~30,000 pre-merge). The migration target tripled. The `tx-policy` and `tx-services` skeletons remain — `SubjectAuthority` / `RestrictionStack` will land in those.

## 2. Surface counts (post-merge)

<!-- txdoc:BLAST-V2-SURFACE-1 -->

### 2.1 StepOutcome surface — fully concentrated on mainline

| Variant | Sites |
|---|---|
| `StepOutcome::Advanced` | 142 |
| `StepOutcome::Blocked` | 152 |
| `StepOutcome::AdvancedThenBlocked` | 129 |
| `StepOutcome::Done` | 649 |
| `StepOutcome::Err` | 609 |
| **Total construction-or-pattern sites** | **~1,512** |
| **Sites needing mechanical refactor (Advanced/Blocked/AdvancedThenBlocked)** | **354** |
| `match` arms over `StepOutcome` | 1,506 |
| Files containing `StepOutcome` | 75 |

Refactor sites split production vs test:

| | Files | Refactor sites |
|---|---|---|
| Production | 44 | 323 |
| Test | 31 | 31 |
| **Total** | **75** | **354** |

Distribution by crate (refactor sites only):

| Crate | Refactor sites |
|---|---|
| `tx-subsystems` | 215 |
| `tx-shims` | 105 |
| `tx-scripts` | 17 |
| `tx-fs` | 10 |
| `tx-kernel` | 7 |
| **Total** | **354** |

`tx-subsystems` carries 60% of the refactor cost; the top three files (`page_backed.rs` 150 occurrences, `tmpfs.rs` 116, `vfs/execution.rs` 107) account for ~25% of total `StepOutcome` surface.

### 2.2 Step-function surface — for `StepOp` trait wrapping

```
135 step_* function signatures across 32 files
```

Adding the `StepOp` trait wraps each as `impl StepOp for FooOp { ... }`, with the function body relocated. Modal cost per step-fn: low, but cumulative for 135 functions.

### 2.3 Other primitive surfaces

| Name | Files | Hits | State |
|---|---|---|---|
| `Cap<` | 89 | 600 | wired throughout |
| `PayloadCap` | 26 | 91 | wired |
| `Weak<` | 10 | 52 | light |
| `OperationalEvidence` | 5 | 17 | light |
| `IdentRef<` | 4 | 13 | light (most witnesses are implicit) |
| `require_*` | 21 | 113 | wired |
| `Reservation` | 4 | 16 | light |
| `Witness` | 0 | 0 | not a named type |
| `ScriptCtx` / `ThreadContext` / `SubjectContext` | 0 | 0 | **net new** |
| `WakeCarrier` / `InterestConditions` | 0 | 0 | **net new** (current code uses subsystem-specific types) |
| `drive_blocking` / `_nonblocking` / `_selecting` | 0 | 0 | **net new** |
| `zone::sign` / `index::commit` | 0 | 0 | **net new vocabulary** (substrate primitives exist under different names) |

**Cap discipline is real and pervasive.** Identity/payload split is wired (`PayloadCap` in 26 files). The witness/IdentRef discipline is *light* (only 4 files name `IdentRef<`), suggesting most observation evidence is implicit in `require_*` returns rather than typed witnesses. The driver-mode catalog and the wake-carrier/interest types are still doc-vocabulary, not code vocabulary.

## 3. Per-change blast radius (post-merge)

<!-- txdoc:BLAST-V2-PER-CHANGE-1 -->

| # | Change | Code surface | Doc surface | New code | Risk |
|---|---|---|---|---|---|
| **A** | `StepOutcome` 5→4 + `YieldShape::OnCarrier` | **354 mechanical refactor sites across 75 files** (323 production + 31 test); ~1,150 unchanged Done/Err sites | `STEP_MODEL_v1` superseded by `03_STEP_MODEL_v2`; tag updates in ~33 v4 docs | ~50 LoC for new types | **Medium** — exhaustive match catches misses at compile time, but test-suite has to be re-greened across 31 test files. |
| **B** | `StepOp` trait + `StepProgress` associated type | **135 step-fn signatures across 32 files wrap into impls** | one new section in `03_STEP_MODEL_v2` | ~150 LoC trait + 5–10 progress types | **Medium** — one wrap per function; per-step body lift. |
| **C** | `YieldShape::OnAgent` (Delegate) | 0 — net new | new `05_DELEGATE_v1` (in this folder); INVARIANTS-V5 YIELD-* + DELEGATE-* | ~500 LoC token zone (`tx-substrate`), ~500 LoC endpoint (`tx-subsystems`), ~200 LoC bus wiring (`tx-reactor`), ~300 LoC drive logic (`tx-scripts`). **~1,500 LoC framework.** | **Medium** — new substrate primitive |
| **D** | `SubjectContext` + upper/lower split | 0 — net new in code (zero hits today) | new `04_SYSCALL_SHAPE_v1`; CONCEPTS-V5 §SUBJ; INVARIANTS-V5 SUBJ-* | ~200 LoC types; ~300 LoC threading through canonical syscalls (sys_open / sys_read / sys_write / sys_fork / sys_execve) | **Low** — additive; lands in tx-policy skeleton + new `tx-shims/src/subject.rs`. |
| **E** | `OnBehalfOf<P>` execution scope | 0 — net new | new `06_EXECUTION_SCOPE_v1`; INVARIANTS-V5 SCOPE-* | ~500 LoC framework (`tx-scripts`) | **Medium** — defers cleanly until first user (AIO or SQPOLL). |
| **F** | `SubjectAuthority.restrictions` cell | 0 — `tx-policy` is still skeleton | new RESTRICTION_v1 (deferred); cred_service v2 doc | ~150 LoC framework cell in `tx-policy`; per-restriction-kind cost is large (seccomp BPF VM ~3-6k LoC) | **Low for cell, high per implementation.** |

### 3.1 What changed vs v1

| Aspect | v1 (pre-merge) | v2 (post-merge) |
|---|---|---|
| Refactor location | mostly worktrees; 8 files in mainline | all on mainline; 75 files |
| Refactor sites | 280 (mainline) + 423 (funny-hugle worktree) = staggered | 354 (single tree) |
| Worktree rebase | 3 branches × ~1.5 days | **gone** |
| Test-suite refactor | small (mainline tests) | **31 test files**, 31 refactor sites |
| Concurrency cost | sequential: foundation → worktree rebase × 3 | sequential within mainline |
| Net new code estimate | ~2.7k LoC for steps 1–3 | ~2.7k LoC unchanged |
| Step-fn wrap surface | ~75 estimated | **135 measured** |

The total mechanical work shrunk slightly (354 vs 280 + 423) because the worktrees overlapped each other's refactor sites; merge consolidated. Coordination cost vanished. Test surface entered the picture as a real cost.

## 4. Implications for landing strategy

<!-- txdoc:BLAST-V2-STRATEGY-1 -->

**The foundation PR is now bigger but more contained.** Pre-merge, the foundation could land on a near-empty `tx-subsystems` and worktrees rebased afterward. Post-merge, the foundation refactor touches 75 files and 354 sites in one tree at one time.

**Two viable paths:**

### Path A — Atomic foundation PR

One large PR: `StepOutcome` 4-variant + `YieldShape::OnCarrier` + `StepOp`/`StepProgress` traits, with all 354 sites rewritten and all 31 test files re-greened. Compile-driven; the rust compiler enumerates every miss.

**Cost:** ~5–8 days of focused work; long PR review window.
**Risk:** large diff, high merge-conflict surface for any concurrent work.
**Mitigation:** lock concurrent subsystem work for the duration; communicate the freeze.

### Path B — Compat-layer transitional shape

Land the new four-variant `StepOutcome` and `YieldShape` alongside the old five-variant via a *compatibility shim*:

```rust
// transitional helpers in tx-substrate (or a new tx-step crate):
impl<T> StepOutcome<T, ByteProgress> {
    pub fn advanced(n: usize) -> Self {
        Continue { progress: ByteProgress(n) }
    }
    pub fn blocked(c: WakeCarrier, m: InterestConditions) -> Self {
        Yield { progress: ByteProgress::EMPTY,
                shape: YieldShape::OnCarrier { carrier: c, interests: m } }
    }
    pub fn advanced_then_blocked(n: usize, c: WakeCarrier, m: InterestConditions) -> Self {
        Yield { progress: ByteProgress(n),
                shape: YieldShape::OnCarrier { carrier: c, interests: m } }
    }
}
```

Old call sites continue to compile via `StepOutcome::advanced(n)` etc. Subsystems migrate to the explicit forms incrementally; a deprecation lint deletes the shim once all sites are converted.

**Cost:** ~1–2 days for the shim; ~3 days × 4 subsystem-week increments to migrate `tx-subsystems`, `tx-shims`, `tx-fs`, `tx-scripts` separately; cleanup once. Spread over weeks rather than concentrated in a window.
**Risk:** dual shape exists for the migration period; reviewers must remember which is canonical.
**Recommendation:** **Path A is cleaner. Path B is the fallback if calendar pressure forbids a focused refactor week.**

### Path A's actual sequence

1. **Day 1.** Land the new `StepOutcome` enum + `YieldShape` enum + `StepProgress` trait (no `StepOp` yet). All 354 sites updated; 31 test files re-greened. Single PR to mainline. ~4–5 days of work, including review.
2. **Day 2.** Land `StepOp` trait wrap. Each step-fn becomes `impl StepOp for FooOp`. ~135 wraps across 32 files. ~3 days of work; can be split into per-subsystem PRs (one for `vfs`, one for `tty`, one for `page_backed`, etc.) since the underlying outcome shape is already migrated.
3. **Day 3.** Land `SubjectContext` + canonical syscall examples in `tx-shims` and `tx-policy`. ~500 LoC; mostly net-new. ~2 days of work.

After step 3, the foundation is in place. Subsequent work (Delegate, OnBehalfOf, restrictions) is feature-gated — each lands when its first user is ready.

## 5. Recommended landing order (post-merge)

<!-- txdoc:BLAST-V2-LANDING-1 -->

1. **PR-1: Foundation algebra refactor.** `StepOutcome` 4-variant, `YieldShape::OnCarrier`, `StepProgress` trait + 5 progress impls. All 354 sites rewritten; tests re-greened. (~5 days, single PR or coordinated PR series.)
2. **PR-2 through PR-N: `StepOp` per-subsystem wrap.** One PR per `tx-subsystems` module (vfs, page_backed, tty, vm, signal, mount, …) wrapping its step-fns. Atomic per subsystem, parallelizable across reviewers. (~3 days total work.)
3. **PR: SubjectContext skeleton.** Lands in `tx-policy` (currently skeleton) and new `tx-shims/src/subject.rs`. Canonical syscall examples in `tx-shims`. (~2 days.)
4. **PR: OnAgent token zone + endpoint zone (no agent kind yet).** Compile-only changes in `tx-substrate` and `tx-subsystems`. (~3 days.)
5. **PR: Driver-mode `Waiting::handle` for OnAgent.** Tested with synthetic kind. (~2 days.)
6. **PR: First agent kind — userfaultfd.** ~1,500 LoC including ufd subsystem. (~1–2 weeks.)
7. **PR: OnBehalfOf framework.** ~500 LoC. (~3 days.)
8. **PR: First OnBehalfOf user — AIO worker.** ~1,000 LoC AIO + integration. (~1 week.)
9. **PR: SubjectAuthority.restrictions cell stub.** ~150 LoC; opens the door for seccomp/landlock work as separate large efforts.
10. **Subsequent: FUSE delegation, fanotify-perm, ptrace, io_uring SQPOLL, seccomp BPF.** Each as its own large effort against a stable framework.

Total framework foundation (PRs 1–3 + 4–5 + 7 + 9): **~6 PRs over ~3 weeks of focused work**, plus ~2 weeks of follow-up for the first canary use case (PR-6 ufd).

## 6. Risk register (updated)

<!-- txdoc:BLAST-V2-RISKS-1 -->

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| PR-1 conflicts with concurrent subsystem work | High | Medium | Lock concurrent edits in `tx-subsystems` / `tx-shims` / `tx-fs` / `tx-scripts` / `tx-kernel` for the foundation week. |
| Test-suite breakage during refactor | High | Low | Compile-driven; each test file's update is mechanical. Allocate explicit re-greening time. |
| `StepProgress` monomorphization bloat | Low | Low | Per-step impl only; bounded instance count. |
| `StepOp` wrap regresses inlining / hot paths | Low | Low | `#[inline]` annotations on small step impls; benchmark `page_backed.rs` and `tmpfs.rs` (heaviest) before/after. |
| `StepOp` per-subsystem PRs conflict with each other | Low | Low | One subsystem per PR; merge in dependency order. |
| OnAgent token zone contention | Low | Medium | Per-endpoint sharded zone if measurements show contention. |
| Authority drift in OnBehalfOf borrows | Medium | Medium | v1 holds snapshot; document the limit; revisit if real use case. |
| Seccomp BPF VM is large | High | Low (for framework) | Land restriction-stack stub first; defer real BPF. |

## 7. What's already in mainline that helps

<!-- txdoc:BLAST-V2-LANDED-1 -->

The merge has wired up the real surface that the v3 framework attaches to:

- **600 `Cap<T>` sites across 89 files.** The retention discipline is real.
- **91 `PayloadCap` sites across 26 files.** The identity/payload split is implemented.
- **113 `require_*` sites across 21 files.** The predicate-and-witness discipline (modulo the witness type itself being implicit) is enforced.
- **Substrate primitives:** EBR, zone, slab, page allocator. ~13k LoC; functioning.
- **Reactor:** wait, scheduler, polling. ~8.4k LoC; functioning.
- **Subsystem code:** 33,979 LoC of actual subsystem implementations to migrate against.

The v3 refactor is *finishing* a partly-built kernel, not starting from scratch.

## 8. What's still skeleton

<!-- txdoc:BLAST-V2-SKELETON-1 -->

| Crate | Lines | Skeleton because… |
|---|---|---|
| `tx-policy` | 9 | Awaits `SubjectAuthority` / `RestrictionStack` / scheduler-policy types. |
| `tx-services` | 7 | Awaits cred-service / rlimit-service / random / time / trace implementations. |

Both are intentionally empty until v3 lands the types they host. PR-3 (SubjectContext) populates `tx-policy`. Service-layer work is later.

## 9. Compared to v1's plan

<!-- txdoc:BLAST-V2-V1-DIFF-1 -->

The v1 landing order was: foundation → worktree rebase × 3 → SubjectContext → OnAgent. The worktree rebase took 3–6 days of coordination cost.

The v2 landing order has no rebase step. The mainline refactor week (~5 days for PR-1, ~3 days for PR-2 series) replaces it directly. Total foundation cost is similar; *coordination* cost dropped substantially. The remaining schedule (PR-4 onward for OnAgent and beyond) is unchanged from v1.

The single biggest shift is **the loss of staging area.** v1 had skeleton crates as a buffer for SubjectContext-shaped additions. v2 still has `tx-policy` and `tx-services` as buffers, but the StepOutcome refactor itself touches 75 files across the active subsystem code with no isolation. Path A's atomic-PR approach is the cleanest response; Path B's compat-layer is the calendar-pressure fallback.

## 10. Success criteria (unchanged)

<!-- txdoc:BLAST-V2-SUCCESS-1 -->

The migration is "done" when:

- All three v3 foundation primitives (`StepOutcome` four-variant, `StepOp`/`StepProgress`, `SubjectContext`) are in mainline.
- All ~135 step-fn sites have been wrapped into `StepOp` impls.
- All ~354 refactor sites compile and tests pass.
- `OnAgent` is callable and userfaultfd works end-to-end.
- `OnBehalfOf` is callable and AIO works end-to-end.
- v3 docs in `Txv3/` are referenced by the v4 docs they supersede.
- A migration ADR in `docs/progress/decisions/` records the landing.
