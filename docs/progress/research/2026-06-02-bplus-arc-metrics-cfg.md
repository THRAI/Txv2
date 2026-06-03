# B+ Recipe Arc Metrics Config Split

Date: 2026-06-02

## Context

The prior B+ recipe pthread comparison was asymmetric: the B+ run emitted
`debug.vm.recipe.bplus.*` shape counters inside the rewrite path, while the
treap reference did not. That makes the measured B+ service regression suspect
until the shape counter overhead is removed from the timed critical section.

The remaining observe-independent facts still hold: B+ reduced recipe
node/chunk allocation counts, but `touched_entries` stayed near the treap
shape, and chunk reclaim was more expensive per retired object. The next
question is whether the remaining service is dominated by `Arc<VmEntry>`
clone/drop churn for surviving entries.

## Change

- Normal `tx_vm_recipe_bplus` builds now keep B+ shape counters off.
- `tx_vm_recipe_bplus_shape_metrics` restores the `debug.vm.recipe.bplus.*`
  shape counters explicitly for shape-only diagnostic runs.
- `tx_vm_recipe_bplus_arc_metrics` wraps B+ leaf `Arc<VmEntry>` entries in a
  timed diagnostic wrapper that emits:
  - `debug.vm.recipe.bplus.entry_arc.new_duration_ns`
  - `debug.vm.recipe.bplus.entry_arc.clone_duration_ns`
  - `debug.vm.recipe.bplus.entry_arc.drop_duration_ns`
- The timed Arc wrapper is attribution-only. It should not be used for B+
  service comparisons because it intentionally adds per-entry observe records.

## Verification

- `RUSTFLAGS="--cfg tx_vm_recipe_bplus" cargo test -p tx-subsystems bplus_ -- --nocapture`
  passed all 18 matched B+ tests.
- `RUSTFLAGS="--cfg tx_vm_recipe_bplus --cfg tx_vm_recipe_bplus_shape_metrics" cargo test -p tx-subsystems bplus_ -- --nocapture`
  passed all 18 matched B+ tests.
- `RUSTFLAGS="--cfg tx_vm_recipe_bplus --cfg tx_vm_recipe_bplus_arc_metrics" cargo test -p tx-subsystems bplus_ -- --nocapture`
  passed all 18 matched B+ tests.
- `cargo fmt --check --package tx-subsystems` passed.
- `cargo check -p tx-subsystems -q` passed.
- `RUSTFLAGS="--cfg tx_vm_recipe_bplus --cfg tx_vm_recipe_bplus_arc_metrics" cargo check -p tx-subsystems -q`
  passed.

## Next Measurement

Use `cargo xtask observe oscomp-live --test pthread` so the shared-memory file,
host drain, and Parquet export are handled by the unified workflow.

Service comparison run, with shape counters off:

```sh
RUSTFLAGS="--cfg tx_lock_metrics --cfg tx_lock_metrics_vm --cfg tx_vm_recipe_bplus" \
  cargo xtask observe oscomp-live \
    --test pthread \
    --output-dir target/oscomp/custom-run/recipe-bplus-equalized-pthread-20260602
```

Arc-churn attribution run, not comparable for service timing:

```sh
RUSTFLAGS="--cfg tx_lock_metrics --cfg tx_lock_metrics_vm --cfg tx_vm_recipe_bplus --cfg tx_vm_recipe_bplus_arc_metrics" \
  cargo xtask observe oscomp-live \
    --test pthread \
    --output-dir target/oscomp/custom-run/recipe-bplus-arcmetrics-pthread-20260602
```

Read `lock_rows.parquet` for `debug.lock.vm.recipe_index.mutation` and
`counters.parquet` for the three `entry_arc.*duration_ns` counters. Compare the
Arc clone/drop event counts against `2 * touched_entries`; if they track, the
next structural lever is reducing survivor ownership churn, not tree depth.

## Failed Capture Attempt

An equalized pthread capture was attempted at:

`target/oscomp/custom-run/recipe-bplus-equalized-pthread-20260602-2050`

It is not performance evidence. The serial log reached
`txkernel:qemu-riscv64-virt:userspace:submitted`, then stopped. QEMU and the
live-drain process stayed alive, but `trace.rawrecords` remained `0B`; after a
manual stop, `runtime.json` reported `raw_records=0`, `complete=true`, and all
four ring producers at zero. The generated Parquet files are empty stubs. Do
not compare this run against the treap reference.

## Successful SMP1 Capture

SMP4 AP-local observe coverage is being fixed in a separate worktree, so the
current valid data point is SMP1:

```sh
RUSTFLAGS="--cfg tx_lock_metrics --cfg tx_lock_metrics_vm --cfg tx_vm_recipe_bplus" \
  python3 tools/oscomp-observe-live.py \
    --test pthread \
    --smp 1 \
    --timeout 900 \
    --output-dir target/oscomp/custom-run/recipe-bplus-equalized-pthread-smp1-20260602-212409
```

Artifact:

`target/oscomp/custom-run/recipe-bplus-equalized-pthread-smp1-20260602-212409`

Runtime quality:

- `raw_records=7,824,330`
- `complete=true`
- `lost_records=0`
- `overwritten_records=0`
- `repairs=0`
- serial reached `#### OS COMP TEST GROUP END libcbench-musl ####` and
  `userspace:exited:0`
- Parquet rows: `spans=156,489`, `counters=5,395,018`,
  `lock_rows=1,078,341`, `allocation_rows=85,056`

Pthread body times from serial:

- `b_pthread_createjoin_serial1`: `56.192957s`
- `b_pthread_createjoin_serial2`: `100.007035s`
- `b_pthread_create_serial1`: `62.412245s`
- `b_pthread_uselesslock`: `0.209741s`
- `b_pthread_createjoin_minimal1`: `70.682575s`
- `b_pthread_createjoin_minimal2`: `75.345875s`

Recipe lock row for `debug.lock.vm.recipe_index.mutation`:

- `service`: `n=30,015`, total `32.485184s`, avg `1.082298ms`,
  p50 `739us`, p95 `2.467ms`, p99 `7.463ms`, max `111.035ms`
- `response`: `n=30,015`, total `32.597589s`, avg `1.086043ms`,
  p50 `743us`, p95 `2.478ms`, p99 `7.466ms`, max `111.047ms`
- `wait`: `n=30,015`, total `112.405ms`, avg `3.745us`, p50 `3us`,
  p95 `7us`, p99 `19us`, max `1.824ms`

Recipe publish counters:

- `debug.vm.recipe.publish.duration_ns`: `n=30,015`, total `26.686791s`,
  avg `889.115us`, p50 `564us`, p95 `2.093ms`, p99 `7.135ms`,
  max `109.626ms`
- `debug.vm.recipe.publish.touched_entries`: `n=30,015`,
  avg `8.533`, p50 `6`, p95 `17`, p99 `22`, max `26`
- `debug.vm.recipe.publish.node_allocs`: `n=30,015`, avg `1.808`,
  p50 `1`, p95 `4`, p99 `5`, max `7`
- `debug.vm.recipe.reclaim_tree.duration_ns`: `n=29,969`,
  total `6.953664s`, avg `232.029us`, p50 `158us`, p95 `573us`,
  p99 `1.021ms`, max `21.454ms`

The `debug.vm.recipe.bplus.*` shape counters are absent in this equalized
service run as intended; they remain opt-in under
`tx_vm_recipe_bplus_shape_metrics`.

## Arc Metrics SMP1 Capture

Arc-churn attribution was rerun with the active wrapper cfg on the pthread
suite at SMP1:

```sh
RUSTFLAGS="--cfg tx_lock_metrics --cfg tx_lock_metrics_vm --cfg tx_vm_recipe_bplus --cfg tx_vm_recipe_bplus_arc_metrics" \
  python3 tools/oscomp-observe-live.py \
    --test pthread \
    --smp 1 \
    --timeout 900 \
    --output-dir target/oscomp/custom-run/recipe-bplus-arcmetrics-pthread-smp1-20260602-215049
```

Artifact:

`target/oscomp/custom-run/recipe-bplus-arcmetrics-pthread-smp1-20260602-215049`

Runtime quality:

- `raw_records=8,910,654`
- `complete=true`
- `lost_records=0`
- `overwritten_records=0`
- `repairs=0`
- Parquet output only; `runtime.json` has `replay_ndjson=null` and
  `pftrace=null`.

Arc metrics were present in this run:

- `debug.vm.recipe.publish.touched_entries`: `n=30,015`,
  total `256,120`, avg `8.533`, p50 `6`, p95 `17`, p99 `22`, max `26`
- `debug.vm.recipe.bplus.entry_arc.clone_duration_ns`: `n=484,737`,
  total `817.946ms`, p50 `1us`, p95 `3us`, p99 `5us`, max `7.784ms`
- `debug.vm.recipe.bplus.entry_arc.drop_duration_ns`: `n=514,464`,
  total `1.573521s`, p50 `2us`, p95 `8us`, p99 `35us`, max `15.032ms`
- `debug.vm.recipe.bplus.entry_arc.new_duration_ns`: `n=29,966`,
  total `359.847ms`, p50 `8us`, p95 `20us`, p99 `57us`, max `4.529ms`

The exact `clone+drop == 2 * touched_entries` model is falsified on this full
pthread capture:

- `clone_events + drop_events = 999,201`
- `2 * touched_entries = 512,240`
- ratio to `2 * touched_entries`: `1.9507`
- ratio to `touched_entries`: `3.9006`

This still supports Arc churn as a real B+ recipe cost: wrapper clone/drop
events are large and roughly linear in touched entries, but the implementation
does more than one survivor clone/drop round per touched entry. Treat the next
representation question as "reduce survivor ownership churn and extra wrapper
traffic", not as a clean two-atomic-per-touched-entry floor.

## Scratch Clone Round Removed

The B+ leaf chunk builder now consumes owned `BPlusEntryRef`s directly into
inline leaf storage instead of building a scratch `Vec<BPlusEntryRef>` and then
cloning that scratch slice into the leaf. A focused host test pins the
mechanism under `tx_vm_recipe_bplus_arc_metrics`:

```sh
RUSTFLAGS="--cfg tx_vm_recipe_bplus --cfg tx_vm_recipe_bplus_arc_metrics" \
  cargo test -p tx-subsystems bplus_owned_leaf_build_does_not_clone_entry_refs -- --nocapture
```

The test failed before the change with two clone records for two owned entries
and passes after the move-based leaf builder with zero clone records during
owned leaf construction.

Functional coverage after the change:

```sh
RUSTFLAGS="--cfg tx_vm_recipe_bplus" \
  cargo test -p tx-subsystems bplus_ -- --nocapture
```

passed all 18 matched B+ tests. A focused formatting check on the touched file
also passed:

```sh
rustfmt --check crates/tx-subsystems/src/vm/structure/recipe_tree.rs
```

### Arc Attribution Rerun

Arc metrics were rerun on full pthread SMP1:

```sh
RUSTFLAGS="--cfg tx_lock_metrics --cfg tx_lock_metrics_vm --cfg tx_vm_recipe_bplus --cfg tx_vm_recipe_bplus_arc_metrics" \
  python3 tools/oscomp-observe-live.py \
    --test pthread \
    --smp 1 \
    --timeout 900 \
    --output-dir target/oscomp/custom-run/recipe-bplus-noscratch-arcmetrics-pthread-smp1-20260602-223145
```

Runtime quality:

- `raw_records=8,893,754`
- `complete=true`
- `lost_records=0`
- `overwritten_records=0`
- `repairs=0`
- Parquet output only; `runtime.json` has `replay_ndjson=null` and
  `pftrace=null`.

Counter comparison against the prior full pthread arc run:

| Metric | Prior | No-scratch |
|---|---:|---:|
| `touched_entries` total | `256,120` | `256,120` |
| publish count | `30,015` | `30,015` |
| `entry_arc.new` events | `29,966` | `29,966` |
| `entry_arc.clone` events | `484,737` | `228,617` |
| `entry_arc.drop` events | `514,464` | `258,344` |
| `clone+drop` events | `999,201` | `486,961` |
| `clone+drop / touched` | `3.9006` | `1.9013` |
| `clone+drop / (2*touched)` | `1.9507` | `0.9507` |

Conclusion: the extra scratch clone/drop round was real and is removed. The
remaining survivor ownership churn is now close to the expected residual
one-clone/one-drop floor for this representation.

### Clean Service Rerun

Shape metrics and Arc metrics were kept off for the service comparison:

```sh
RUSTFLAGS="--cfg tx_lock_metrics --cfg tx_lock_metrics_vm --cfg tx_vm_recipe_bplus" \
  python3 tools/oscomp-observe-live.py \
    --test pthread \
    --smp 1 \
    --timeout 900 \
    --output-dir target/oscomp/custom-run/recipe-bplus-noscratch-equalized-pthread-smp1-20260602-224102
```

Runtime quality:

- `raw_records=8,064,934`
- `complete=true`
- `lost_records=0`
- `overwritten_records=0`
- `repairs=0`
- serial reached `#### OS COMP TEST GROUP END libcbench-musl ####` and
  `userspace:exited:0`
- Parquet output only; `runtime.json` has `replay_ndjson=null` and
  `pftrace=null`.

Recipe mutation lock, compared to the prior equalized SMP1 run:

| Metric | Prior | No-scratch | Delta |
|---|---:|---:|---:|
| service total | `32.485184s` | `26.376291s` | `-18.8%` |
| service avg | `1.082298ms` | `878.770us` | `-18.8%` |
| service p50 | `739us` | `685us` | `-7.3%` |
| service p95 | `2.467ms` | `1.807ms` | `-26.8%` |
| service p99 | `7.463ms` | `4.960ms` | `-33.5%` |
| response avg | `1.086043ms` | `882.639us` | `-18.7%` |
| wait total | `112.405ms` | `116.118ms` | `+3.3%` |

Publish and reclaim counters:

| Metric | Prior | No-scratch | Delta |
|---|---:|---:|---:|
| publish total | `26.686791s` | `21.202320s` | `-20.6%` |
| publish avg | `889.115us` | `706.391us` | `-20.6%` |
| publish p95 | `2.093ms` | `1.496ms` | `-28.5%` |
| publish p99 | `7.135ms` | `4.764ms` | `-33.2%` |
| touched total | `256,120` | `256,120` | unchanged |
| node allocs total | `54,277` | `54,277` | unchanged |
| reclaim total | `6.953664s` | `6.616820s` | `-4.8%` |
| reclaim p99 | `1.021ms` | `863us` | `-15.8%` |

The service result is a real improvement but it does not meet the old
promotion target. Against the equalized B+ SMP1 baseline it is just under the
20% service-gate threshold, and it is still well above the earlier target
average of `<=516us`. The next representation question should therefore start
from the residual `~1.9` Arc clone/drop events per touched entry.

## Survivor Sharing Unit Fix

The next VM-local cleanup stopped owning unchanged survivor entries during B+
`replace_range` leaf rewrites. `BPlusLeaf` now stores inline leaf segments:

- `Owned(BPlusEntryRef)` for new or replacement entries.
- `SharedRun { leaf, start, len }` for unchanged survivors borrowed from an
  old immutable leaf.

The replacement path flushes survivor runs at overlap and insertion boundaries,
so replacements stay sorted without cloning the unchanged entries around them.
When a rewritten leaf overflows, the chunker splits shared runs at leaf
boundaries instead of flattening them back through `Arc<VmEntry>` clones.

Host unit tests for `tx_vm_recipe_bplus_arc_metrics` now use local atomic clone
counters instead of observe records. Test builds bypass the timing-record emit
inside `time_bplus_entry_arc_op`, while non-test arc-metric builds still emit
`debug.vm.recipe.bplus.entry_arc.*duration_ns` for OSComp attribution runs.
This avoids host test threads entering `tx_observe::current()` with non-kernel
hart ids.

The linter sweep also found and removed one VM-local treap redundant clone in
`TreapRecipeIndex::insert_entry`. Remaining `clippy::redundant_clone` findings
from the requested pass were outside VM (`process/execution.rs` and
`vfs/resolution/driver.rs`) and were left out of this VM-first patch.

Verification:

```sh
RUSTFLAGS="--cfg tx_vm_recipe_bplus --cfg tx_vm_recipe_bplus_arc_metrics" \
  cargo test -p tx-subsystems bplus_owned_leaf_build_does_not_clone_entry_refs -- --nocapture

RUSTFLAGS="--cfg tx_vm_recipe_bplus --cfg tx_vm_recipe_bplus_arc_metrics" \
  cargo test -p tx-subsystems bplus_replace_range_shares_unchanged_survivor_refs -- --nocapture

RUSTFLAGS="--cfg tx_vm_recipe_bplus" \
  cargo test -p tx-subsystems bplus_ -- --nocapture

rustfmt --edition 2024 --check crates/tx-subsystems/src/vm/structure/recipe_tree.rs
git diff --check -- crates/tx-subsystems/src/vm/structure/recipe_tree.rs

RUSTFLAGS="--cfg tx_vm_recipe_bplus" \
  cargo clippy -p tx-subsystems --lib -- -W clippy::redundant_clone
```

Results:

- Both focused arc-counter tests passed. The survivor replacement test now
  proves the clone count is unchanged across a single-leaf replacement.
- The normal B+ suite passed all 18 matched tests, including the spanning
  locality guardrail.
- The touched-file rustfmt and diff checks passed.
- The clippy pass completed with no VM `redundant_clone` findings remaining.
  It still reports unrelated non-VM redundant clones plus existing style
  warnings.
