# mmap/munmap map-path probe

Date: 2026-06-04

## Scope

This pass added a narrow observe gate for mmap/munmap attribution in the pthread
VM spine. The goal was to split `mmap` and `munmap` into path-level costs before
choosing another data-structure or shootdown optimization.

The new cfg is `tx_vm_map_path_metrics`. It emits counters only when an observer
is active.

Touched probe sites:

- `AddressSpace::try_mmap`: free-range search, reservation, commit, changed
  pages, total.
- `AddressSpace::commit_reserved_map`: recipe commit, fixed-replace pmap
  teardown, stats.
- `AddressSpace::try_munmap`: range-lock acquire, recipe unmap, pmap teardown,
  stats, changed pages, pmap removed pages, total.
- `VmPmap::teardown_range`: resident-store drain, removed page count, shifted
  entry count, HAL unmap loop, shootdown batch, total.

Stable names were registered in `xtask/src/observe.rs` and
`tools/tx-observe-analyze.py`.

## Verification

Code and tooling checks run for this probe:

- `python3 -m py_compile tools/tx-observe-analyze.py`
- `cargo check -p tx-subsystems -q`
- `RUSTFLAGS="--cfg tx_vm_map_path_metrics --cfg tx_vm_recipe_bplus --cfg tx_lock_metrics --cfg tx_lock_metrics_vm" cargo check -p tx-subsystems -q`
- `cargo check -p xtask -q`
- `rustfmt --edition 2021 crates/tx-subsystems/src/vm/execution.rs crates/tx-subsystems/src/vm/pmap.rs xtask/src/observe.rs`
- `rustfmt --edition 2021 --check crates/tx-subsystems/src/vm/execution.rs crates/tx-subsystems/src/vm/pmap.rs xtask/src/observe.rs`
- `git diff --check -- crates/tx-subsystems/src/vm/execution.rs crates/tx-subsystems/src/vm/pmap.rs xtask/src/observe.rs tools/tx-observe-analyze.py Cargo.toml`

## Artifact

Primary observe artifact:

`target/oscomp/custom-run/map-path-mm-io-pthread-20260604`

Command:

```sh
RUSTFLAGS="--cfg tx_lock_metrics --cfg tx_lock_metrics_vm --cfg tx_lock_metrics_fs --cfg tx_vm_recipe_bplus --cfg tx_vm_map_path_metrics" \
  python3 tools/oscomp-observe-live.py \
  --test mm-io-pthread \
  --output-dir target/oscomp/custom-run/map-path-mm-io-pthread-20260604
```

Runtime quality:

- `raw_records=4,802,275`
- `complete=false`
- `lost_records=1,401`
- `overwritten_records=0`
- `repairs=0`

This is usable for ranking and phase attribution with a caveat, but it is not a
lossless capture.

Probe timings are not score timings. The serial body times in this run are
probe-inflated, especially in pthread and stdio.

## Main phase totals

Map-path duration counters from `analysis/parquet/counters.parquet`:

| counter | samples | total | avg | p50 | p99 | max |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `debug.vm.map_path.munmap.total_ns` | 11143 | 19.220s | 1724.828us | 903us | 20.995ms | 93.488ms |
| `debug.vm.map_path.mmap.total_ns` | 14003 | 14.646s | 1045.920us | 840us | 5.064ms | 41.128ms |
| `debug.vm.map_path.pmap.teardown_total_ns` | 18664 | 9.693s | 519.356us | 208us | 3.254ms | 87.513ms |
| `debug.vm.map_path.mmap.commit_ns` | 13999 | 9.385s | 670.390us | 522us | 3.437ms | 40.335ms |
| `debug.vm.map_path.munmap.pmap_teardown_ns` | 11143 | 9.373s | 841.176us | 372us | 15.494ms | 87.573ms |
| `debug.vm.map_path.munmap.recipe_ns` | 11143 | 8.515s | 764.149us | 393us | 5.889ms | 68.429ms |
| `debug.vm.map_path.mmap.commit_recipe_ns` | 13999 | 8.182s | 584.456us | 446us | 2.657ms | 40.212ms |
| `debug.vm.map_path.mmap.reserve_map_ns` | 13999 | 2.408s | 172.024us | 135us | 610us | 26.809ms |
| `debug.vm.map_path.pmap.teardown_drain_ns` | 18664 | 2.382s | 127.645us | 52us | 360us | 27.810ms |
| `debug.vm.map_path.mmap.anywhere_search_ns` | 13967 | 1.782s | 127.575us | 100us | 437us | 3.811ms |
| `debug.vm.map_path.pmap.teardown_hal_unmap_ns` | 39866 | 1.663s | 41.707us | 19us | 249us | 13.764ms |
| `debug.vm.map_path.pmap.teardown_shootdown_ns` | 18664 | 1.524s | 81.629us | 68us | 439us | 8.095ms |

Shape counters:

| counter | samples | total | avg | p50 | p90 | p99 | max |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `debug.vm.map_path.pmap.teardown_removed_pages` | 18664 | 39866 | 2.136 | 1 | 1 | 32 | 170 |
| `debug.vm.map_path.pmap.teardown_shifted_entries` | 18664 | 5940620 | 318.293 | 0 | 47 | 10021 | 11295 |
| `debug.vm.map_path.mmap.changed_pages` | 13999 | 10358230 | 739.926 | 7 | 2051 | 2051 | 2051 |
| `debug.vm.map_path.munmap.changed_pages` | 11143 | 10316621 | 925.839 | 32 | 2051 | 2051 | 2051 |
| `debug.vm.map_path.munmap.pmap_removed` | 11143 | 39866 | 3.578 | 1 | 1 | 32 | 170 |

## Syscall context

The same window ranks syscall spans as:

| syscall | n | total | avg | p50 | p99 | max |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `sys_clone` | 12521 | 23.610s | 1885.603us | 1584us | 6.313ms | 409.921ms |
| `sys_munmap` | 11143 | 20.983s | 1883.083us | 1046us | 21.365ms | 93.706ms |
| `sys_mmap` | 13977 | 15.904s | 1137.841us | 918us | 5.313ms | 41.394ms |
| `sys_futex` | 19068 | 13.431s | 704.350us | 178us | 5.685ms | 2191.771ms |
| `sys_rt_sigprocmask` | 50124 | 11.552s | 230.466us | 189us | 813us | 50.351ms |
| `sys_mprotect` | 7511 | 9.457s | 1259.103us | 1043us | 5.200ms | 17.632ms |
| `sys_exit` | 12505 | 6.635s | 530.604us | 460us | 1.628ms | 28.856ms |

This confirms the pthread lifecycle VM spine is still central: `munmap` and
`mmap` are directly below `clone` in syscall total.

## Bucketed attribution

### munmap by changed pages

| bucket | n | total | avg | p50 | p99 | avg recipe | avg pmap | avg acquire | avg removed |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `<=8` | 5297 | 4.629s | 873.897us | 705us | 2.528ms | 458.208us | 317.160us | 40.848us | 1.038 |
| `<=32` | 739 | 4.264s | 5769.831us | 2548us | 36.831ms | 2683.126us | 2940.277us | 61.940us | 25.924 |
| `<=256` | 102 | 3.236s | 31.724ms | 30.371ms | 88.725ms | 1784.098us | 29.798ms | 60.422us | 100.049 |
| `>1024` | 5005 | 7.091s | 1416.782us | 1063us | 5.752ms | 783.811us | 495.698us | 61.971us | 1.000 |

Interpretation: large 2051-page stack-sized unmaps are common but usually have
only one resident pmap removal, so they are not the worst per call. The high
tail is mid-sized unmaps with tens to hundreds of resident pmap removals.

### mmap by changed pages

| bucket | n | total | avg | p50 | p99 | avg search | avg reserve | avg commit | avg recipe |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `<=8` | 7904 | 7.473s | 945.489us | 762us | 3.176ms | 125.284us | 173.293us | 574.221us | 501.907us |
| `<=32` | 886 | 2.133s | 2407.153us | 1221us | 20.823ms | 129.414us | 447.910us | 1781.721us | 1669.463us |
| `<=256` | 204 | 0.389s | 1907.544us | 1153us | 15.349ms | 119.113us | 273.755us | 1369.892us | 1283.510us |
| `>1024` | 5005 | 4.648s | 928.734us | 855us | 2.600ms | 130.397us | 117.035us | 597.019us | 494.256us |

Interpretation: `mmap` is recipe-commit dominated. Free-range search is visible
but not the leading cost, and stack-sized maps are cheap relative to mid-sized
recipe tails.

### pmap teardown by removed pages

| bucket | n | total | avg | p50 | p99 | avg drain | avg shootdown | avg shifted | p99 shifted |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `removed=0` | 7521 | 0.746s | 99.208us | 87us | 308us | 47.358us | 3.251us | 0.000 | 0 |
| `removed=1` | 10244 | 3.730s | 364.095us | 321us | 1.096ms | 72.649us | 107.308us | 33.565 | 64 |
| `removed<=8` | 137 | 0.093s | 680.066us | 477us | 3.023ms | 324.328us | 139.307us | 8348.350 | 11251 |
| `removed<=32` | 665 | 2.101s | 3158.803us | 1440us | 25.967ms | 326.068us | 265.884us | 5880.125 | 11212 |
| `removed>32` | 97 | 3.024s | 31.171ms | 29.859ms | 87.513ms | 10.523ms | 2102.216us | 5595.608 | 10484 |

Interpretation: pmap teardown tail is not just shootdown. The resident-store
drain and sorted-store shifting are the strongest pmap-side signatures; HAL
unmap and shootdown matter, but are smaller than total teardown in the largest
tail bucket.

## Tail examples

Worst `munmap.total_ns` rows:

| seq | total | recipe | pmap | changed pages | pmap removed |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 840 | 93.488ms | 19.969ms | 73.387ms | 256 | 138 |
| 110 | 93.182ms | 62.733ms | 30.254ms | 32 | 32 |
| 827 | 88.725ms | 1.040ms | 87.573ms | 256 | 138 |
| 812 | 76.087ms | 446us | 75.354ms | 256 | 160 |
| 624 | 69.665ms | 68.429ms | 932us | 4 | 4 |

This shows two independent `munmap` tail modes:

- pmap-heavy tails when many resident mappings are removed.
- recipe-heavy tails even with few changed pages and few resident pmap removals.

The latter needs a narrower B+ recipe unmap/commit split before choosing a
structure change.

## Invalid follow-up captures

Two follow-up attempts were intentionally excluded from attribution:

- `target/oscomp/custom-run/map-path-malloc-vm-20260604`: booted and submitted
  libcbench, but serial stopped after `terminal_drained=1`; `trace.rawrecords`
  stayed empty and no `runtime.json` was written before the run was terminated.
- `target/oscomp/custom-run/map-path-malloc-big1-20260604`: reproduced the same
  early stall. The live drain finalized only `raw_records=66`,
  `complete=true`, `lost=0`, with no benchmark body lines. This is a runner or
  probe-window failure, not VM attribution evidence.

## Recipe phase follow-up

Follow-up instrumentation added `tx_vm_recipe_phase_metrics` to split the
recipe publication path into:

- mutation lock wait
- rewrite
- root publish swap
- publish debug emission
- EBR retire enqueue
- deferred reclaim enqueue
- deferred reclaim drain
- actual tree drop/reclaim

The same pass added `debug.vm.map_path.pmap.teardown_loop_ns` around the
resident teardown loop so the pmap total can be reconciled against drain, HAL
unmap, and shootdown.

Focused pthread artifact:

`target/oscomp/custom-run/map-path-recipe-phase-pthread-serial1-20260604`

Command:

```sh
RUSTFLAGS="--cfg tx_lock_metrics --cfg tx_lock_metrics_vm --cfg tx_vm_recipe_bplus --cfg tx_vm_map_path_metrics --cfg tx_vm_recipe_phase_metrics" \
  python3 tools/oscomp-observe-live.py \
  --test pthread-serial1 \
  --output-dir target/oscomp/custom-run/map-path-recipe-phase-pthread-serial1-20260604
```

Runtime quality:

- `raw_records=774,716`
- `complete=true`
- `lost_records=0`
- `overwritten_records=0`
- `repairs=0`
- serial body line: `b_pthread_createjoin_serial1 (0)` time `46.310305000`

The standard analyzer/parquet export did not complete on this host after the
guest exited, so this note uses a lightweight raw-record counter decoder over
the live-drain format (`8` byte per-entry header + `80` byte trace record).
Derived JSON summaries are under the run's `analysis/` directory:

- `counter-summary.json`
- `map-path-recipe-phase-report.json`
- `recipe-phase-op-pair-report.json`

The decode itself found `774,716` valid records, zero bad records, and
`208,408` counter rows.

### Clean pthread-serial1 map-path totals

| counter | samples | total | avg | p50 | p99 | max |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `debug.vm.map_path.munmap.total_ns` | 2500 | 3.130s | 1251.826us | 1100us | 3.279ms | 10.323ms |
| `debug.vm.map_path.mmap.total_ns` | 2500 | 2.015s | 805.903us | 660us | 2.314ms | 28.285ms |
| `debug.vm.map_path.munmap.recipe_ns` | 2500 | 1.373s | 549.140us | 472us | 1.624ms | 8.036ms |
| `debug.vm.map_path.munmap.pmap_teardown_ns` | 2500 | 1.385s | 553.949us | 476us | 1.487ms | 6.651ms |
| `debug.vm.map_path.mmap.commit_recipe_ns` | 2500 | 1.105s | 442.156us | 358us | 1.426ms | 11.991ms |
| `debug.vm.map_path.pmap.teardown_total_ns` | 5000 | 1.548s | 309.655us | 328us | 1.202ms | 6.503ms |
| `debug.vm.map_path.pmap.teardown_loop_ns` | 5000 | 0.548s | 109.502us | 127us | 528us | 5.350ms |
| `debug.vm.map_path.pmap.teardown_hal_unmap_ns` | 2500 | 0.428s | 171.169us | 148us | 431us | 5.110ms |
| `debug.vm.map_path.pmap.teardown_drain_ns` | 5000 | 0.350s | 69.914us | 62us | 262us | 5.339ms |
| `debug.vm.map_path.pmap.teardown_shootdown_ns` | 5000 | 0.343s | 68.509us | 54us | 342us | 803us |

This clean focused run does not reproduce the earlier broad-window 68ms
`munmap.recipe_ns` tail. The largest `munmap.recipe_ns` sample is `8.036ms`,
and the largest `mmap.commit_recipe_ns` sample is `11.991ms`. Treat the
earlier complete=false broad-window tails as ranking evidence only.

## 2026-06-05 continuation: other backend target

After the `rt_sigprocmask` probe showed its user-copy max was a rare spike
(`read_user_ns` p99 stayed in the hundreds of microseconds), the next backend
checked was the pthread VM map/unmap spine.

Two artifacts were used with different confidence levels:

- `target/oscomp/custom-run/whole-suite-mm-io-pthread-20260604`: complete
  SMP4 whole-suite ranking (`complete=true`, zero lost/overwritten/repairs).
  It does not contain `debug.vm.map_path.*` names, so it ranks syscall families
  but cannot decompose VM internals.
- `target/oscomp/custom-run/map-path-mm-io-pthread-20260604`: detailed
  map-path decomposition (`debug.vm.map_path.*`) but `complete=false` with
  `lost_records=1,401`; use it for shape and backend attribution, not absolute
  score timing.

The complete whole-suite ranking still puts the map/unmap backend just behind
clone:

| syscall | n | total | p50 | p99 | max |
| --- | ---: | ---: | ---: | ---: | ---: |
| `sys_clone` | 12521 | 14.781s | 1.044ms | 4.509ms | 238.955ms |
| `sys_munmap` | 11094 | 11.696s | 610us | 15.804ms | 64.286ms |
| `sys_mmap` | 13929 | 9.126s | 549us | 1.942ms | 20.622ms |
| `sys_mprotect` | 7511 | 5.432s | 632us | 4.372ms | 7.770ms |

The detailed map-path run says the recurring map/unmap tail is mostly
`munmap`, and inside `munmap` the p99 is pmap teardown:

| counter | samples | total | p50 | p99 | max |
| --- | ---: | ---: | ---: | ---: | ---: |
| `debug.vm.map_path.munmap.total_ns` | 11143 | 19.220s | 903us | 20.995ms | 93.488ms |
| `debug.vm.map_path.munmap.pmap_teardown_ns` | 11143 | 9.373s | 372us | 15.494ms | 87.573ms |
| `debug.vm.map_path.munmap.recipe_ns` | 11143 | 8.515s | 393us | 5.889ms | 68.429ms |
| `debug.vm.map_path.pmap.teardown_total_ns` | 18664 | 9.693s | 208us | 3.254ms | 87.513ms |
| `debug.vm.map_path.pmap.teardown_drain_ns` | 18664 | 2.382s | 52us | 360us | 27.810ms |
| `debug.vm.map_path.pmap.teardown_hal_unmap_ns` | 39866 | 1.663s | 19us | 249us | 13.764ms |
| `debug.vm.map_path.pmap.teardown_shootdown_ns` | 18664 | 1.524s | 68us | 439us | 8.095ms |

Joining the slowest `sys_munmap` spans to in-span map-path counters shows two
tail modes, with pmap-heavy calls dominating most of the top samples:

| rank | span | recipe | pmap | drain | HAL unmap | shootdown | shifted | removed | changed |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 93.706ms | 19.969ms | 73.387ms | 21.865ms | 1.615ms | 6.301ms | 4141 | 138 | 256 |
| 2 | 93.476ms | 62.733ms | 30.254ms | 314us | 1.188ms | 353us | 6357 | 32 | 32 |
| 3 | 88.910ms | 1.040ms | 87.573ms | 20.070ms | 2.064ms | 5.190ms | 5687 | 138 | 256 |
| 4 | 76.426ms | 446us | 75.354ms | 26.809ms | 3.356ms | 5.015ms | 7319 | 160 | 256 |
| 5 | 75.234ms | 347us | 74.419ms | 24.763ms | 5.451ms | 4.984ms | 7849 | 160 | 256 |
| 7 | 70.103ms | 68.429ms | 932us | 200us | 163us | 250us | 138 | 4 | 4 |

Readout: the "other backend" is not signal and not purely recipe; it is the
pmap resident teardown backend. The pmap-heavy mode is tied to
`VmPmap::teardown_range`: `ResidentMappings::drain_range` removes a sorted-Vec
slice, returns the tail shift count, then the loop calls `unmap_tracked_page`
for each resident mapping and finally `issue_unmap_batch`. The largest pmap
tails are not explained by shootdown alone; drain/shift plus the unmap loop are
larger than `teardown_shootdown_ns` in the top samples.

Candidate next work: add a focused lossless pthread run with pmap teardown
subphases enabled and, if the shape holds, replace the resident mapping store
with a non-shifting or segmented layout for middle/front removals, or partition
resident mappings per mapping/range so stack teardown does not shift a global
address-sorted Vec. ASID/shootdown targeting remains useful, but it is second to
resident-store teardown for this specific tail.

### Recipe phase split

| counter | samples | total | avg | p50 | p95 | p99 | max |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `debug.vm.recipe.phase.rewrite_ns` | 7502 | 2.827s | 376.835us | 315us | 762us | 1.369ms | 8.363ms |
| `debug.vm.recipe.phase.deferred_drain_ns` | 2499 | 0.842s | 336.945us | 275us | 657us | 1.103ms | 7.389ms |
| `debug.vm.recipe.phase.reclaim_drop_ns` | 7505 | 0.402s | 53.569us | 36us | 134us | 256us | 6.391ms |
| `debug.vm.recipe.phase.debug_emit_ns` | 7502 | 0.255s | 34.016us | 28us | 69us | 125us | 1.116ms |
| `debug.vm.recipe.phase.retire_enqueue_ns` | 7502 | 0.199s | 26.563us | 19us | 61us | 117us | 925us |
| `debug.vm.recipe.phase.deferred_enqueue_ns` | 7505 | 0.150s | 19.945us | 10us | 36us | 92us | 18.099ms |
| `debug.vm.recipe.phase.lock_wait_ns` | 7502 | 0.116s | 15.476us | 13us | 29us | 65us | 1.088ms |
| `debug.vm.recipe.phase.publish_swap_ns` | 7502 | 0.085s | 11.267us | 9us | 22us | 51us | 1.052ms |

Interpretation:

1. The map-path `recipe_ns` counters do not include deferred root drops. In
   code, `munmap.recipe_ns` and `mmap.commit_recipe_ns` wrap
   `RecipeIndex::unmap` / `commit_map`; `drain_deferred_recipe_reclaims` runs
   later from terminal/idle/shutdown drain sites. So reclaim can interfere with
   wall time, but it is not what directly "destroyed" `mmap.commit_recipe_ns`.
2. For the clean pthread-serial1 map path, rewrite is still the largest recipe
   phase. Publish swap, retire enqueue, and debug emission are secondary.
3. Deferred drain/reclaim is real (`0.842s + 0.402s`) and should remain a
   separate return-path/interference lane, but this capture does not support
   treating it as the primary `mmap`/`munmap` recipe counter.
4. `pmap.teardown_loop_ns` closes most of the previous pmap accounting gap:
   pmap teardown now decomposes into resident drain (`0.350s`), loop/HAL/pin
   work (`0.548s`, with HAL unmap `0.428s` inside that loop), and shootdown
   (`0.343s`).

Limitations:

- `debug.vm.recipe.publish.*` names are absent from this focused artifact, so
  this run cannot split rewrite by `MapRequireFree`/`Protect`/`Unmap` publish
  op. A follow-up op-shape run needs the stable publish counters enabled in the
  same narrow profile.
- A broad `--test pthread` run, with and without `tx_vm_recipe_phase_metrics`,
  panicked before the benchmark body with `zone Cap key no longer resolves to a
  live slot`; this is not caused by the phase counters. Use focused pthread
  selectors until that independent broad-selector issue is fixed.

## Conclusions

1. `munmap` is not a single bottleneck. Total time splits between recipe rewrite
   and pmap teardown (`8.515s` vs `9.373s` in the valid window).
2. `mmap` is recipe-commit dominated. Search/reservation is measurable but
   secondary (`commit_recipe=8.182s`, `reserve_map=2.408s`,
   `anywhere_search=1.782s`).
3. Pmap teardown tails are driven by resident mappings removed and sorted-store
   drain/shift work. Shootdown contributes (`1.524s` total), but is not the
   whole tail.
4. Large stack-sized map/unmap operations are common but often cheap per call
   because they touch many recipe pages while removing few resident pmap
   entries. Mid-sized unmaps with many resident pmap removals are the costly
   pmap path.
5. There are recipe-only `munmap` tails with small changed-page counts. The next
   probe should split B+ recipe `unmap`/`commit_map` internals rather than
   assuming all remaining `munmap` cost is shootdown or resident-store work.
6. The lossless pthread-serial1 phase split narrows that recipe lane: the
   syscall-level `mmap`/`munmap` recipe counters are still primarily rewrite,
   while deferred root drop/reclaim is visible but outside those map-path
   counters.

## Next probes

- Split B+ recipe rewrite internals for `MapRequireFree`, `Protect`, and
  `Unmap`: leaf/path copy, entry coalescing, separator rebuild, EBR retire
  enqueue, and any allocation/refcount-heavy sections.
- Re-enable stable recipe publish-op counters in the narrow
  pthread-serial1/map-path profile so rewrite cost can be attributed by
  `MapRequireFree`, `Protect`, and `Unmap` without turning on the heavier shape
  metrics.
- Treat deferred recipe reclaim as a separate interference lane: measure
  terminal/idle drain placement and syscall-return adjacency before moving more
  destruction off path.
- Reproduce the malloc-only observe stall separately before using malloc-only
  windows for attribution. The repeated signature is "userspace submitted,
  terminal drained once, no benchmark body, no meaningful raw records."
