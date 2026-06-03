# libcbench VM-only focused trace

Date: 2026-05-31

Scope: focused bracketed `tx-observe` runs for the libc-bench malloc VM cases:
`b_malloc_sparse`, `b_malloc_bubble`, `b_malloc_big1`, and `b_malloc_big2`.
The runs used the private submit tree
`target/oscomp/custom-run/malloc-vm-submit` and per-case sdcard/output paths
under `target/oscomp/custom-run/`.

## Commands

The four guest runs used this shape, replacing the selector and output stem per
case:

```sh
python3 tools/oscomp-custom-run.py \
  --libcbench \
  --libcbench-only malloc-sparse \
  --observe-bracket \
  --run \
  --skip-build \
  --skip-submit \
  --timeout 240 \
  --serial target/oscomp/custom-run/malloc-sparse-vmtrace-local-serial.txt \
  --data target/oscomp/custom-run/malloc-sparse-vmtrace-local-data \
  --submit target/oscomp/custom-run/malloc-vm-submit \
  --build-dir target/oscomp/custom-run/build-malloc-sparse-vmtrace-local \
  --fault-decode
```

The traces were extracted, validated, and analyzed with:

```sh
cargo xtask observe extract --serial <serial.txt> --output <trace.txtrace>
cargo xtask observe validate --file <trace.txtrace>
cargo xtask observe analyze --file <trace.txtrace> \
  --names target/oscomp/custom-run/malloc-vm-names.json \
  --top 30 > <analyze.txt>
```

## Artifacts

- `target/oscomp/custom-run/malloc-sparse-vmtrace-local-serial.txt`
- `target/oscomp/custom-run/malloc-sparse-vmtrace-local.txtrace`
- `target/oscomp/custom-run/malloc-sparse-vmtrace-local-analyze.txt`
- `target/oscomp/custom-run/malloc-bubble-vmtrace-local-serial.txt`
- `target/oscomp/custom-run/malloc-bubble-vmtrace-local.txtrace`
- `target/oscomp/custom-run/malloc-bubble-vmtrace-local-analyze.txt`
- `target/oscomp/custom-run/malloc-big1-vmtrace-local-serial.txt`
- `target/oscomp/custom-run/malloc-big1-vmtrace-local.txtrace`
- `target/oscomp/custom-run/malloc-big1-vmtrace-local-analyze.txt`
- `target/oscomp/custom-run/malloc-big2-vmtrace-local-serial.txt`
- `target/oscomp/custom-run/malloc-big2-vmtrace-local.txtrace`
- `target/oscomp/custom-run/malloc-big2-vmtrace-local-analyze.txt`
- `target/observe-analyze/malloc-*-vmtrace-local.ndjson`

All four `.txtrace` files validated as `1 hart, 16384 slots/hart, 16384
records, 0 framing errors`. Fault decode found no trap lines in any serial log.

## Results

| Case | Bench time | Retained trace window | VM syscall sample | Page faults in sample | Main VM counters in sample |
| --- | ---: | ---: | --- | ---: | --- |
| sparse | 9.276908s | 491.361ms | `mmap` 33 / 36.320ms total, `brk` 2 / 0.989ms | 58 | private-anon write miss 370 phase groups; pmap publish insert 329 |
| bubble | 10.412752s | 517.205ms | `mmap` 33 / 41.696ms total, `brk` 2 / 1.157ms | 59 | private-anon write miss 372 phase groups; pmap publish insert 317-318 |
| big1 | 17.645432s | 607.630ms | `mmap` 34 / 56.056ms total, `brk` 2 / 2.150ms | 65 | private-anon write miss 365 phase groups; pmap publish insert 311 |
| big2 | 8.741431s | 570.599ms | `mmap` 34 / 53.971ms total, `brk` 2 / 0.853ms | 65 | private-anon write miss 365 phase groups; pmap publish insert 310-311 |

The benchmark bodies are:

- sparse/bubble: 10,000 allocations of 4000 bytes with `memset`, then different
  free patterns.
- big1/big2: 2,000 allocations of 16-64 KiB with no memset, then sequential or
  permuted free.

Each libc-bench `RUN` forks a child, times the benchmark body, then reads
`/proc/self/smaps` in `print_stats`. The bracket therefore covers child setup,
the benchmark body, and the `smaps` tail.

## Attribution

The retained windows are not lossless full benchmark windows. The ring contains
the most recent 16,384 records, which covers only about 0.49-0.61s of each
9-18s benchmark. Treat these traces as VM-phase samples, not full-run totals.

Within those samples, the intrinsic VM cost is not primarily raw `brk` or
`mmap` syscall entry cost. `mmap` accounts for roughly 36-56ms of each retained
window, and `brk` is only 0.9-2.2ms. The slowest VM gaps are mostly:

- `debug.pagebacked.fault_step.kind=2 -> done=2`: 12-28ms outliers.
- `debug.vm.user.pagebacked.phase=0 -> phase=1`: 12-15ms outliers.
- `debug.vm.private_set.install.phase=2 -> phase=3`: up to about 6ms.
- `debug.vm.fault.publish.phase=3 -> phase=4`: up to about 13.7ms.

So the VM-only malloc path still has two different costs:

1. Private-anonymous write faults from heap growth and touched allocation
   pages. These show up as repeated `private_anon.write_miss`,
   `private_set.install`, `fault.prefault`, and `pmap.publish_batch` counters.
2. File-backed executable/smaps-tail faults. The largest sampled gaps are often
   PageBacked fault-step gaps, so some retained-window cost is still the
   libc-bench child/setup or `/proc/self/smaps` tail rather than allocator body
   private-anon work.

The big1/big2 cases are not "no VM" despite the lack of `memset`: the allocator
still expands address space and triggers sampled private-anon/page-table publish
work, but the lower per-case fault density versus sparse/bubble is consistent
with fewer directly touched allocation pages.

## Next step

For exact intrinsic VM totals, add a reduced custom malloc probe with
trace-on/off around only the allocation/free loop and either reset/dump the ring
per phase or use live observe draining. The existing bracketed libc-bench path is
good for structural attribution but cannot prove whole-run totals at the current
ring size.

## Poll-attribution follow-up

Added diagnostic reactor poll markers and analyzer support to classify selected
VM phase gaps as same-poll work versus cross-poll scheduler latency. The focused
follow-up run used:

```sh
python3 tools/oscomp-custom-run.py \
  --libcbench \
  --libcbench-only malloc-sparse \
  --observe-bracket \
  --run \
  --skip-build \
  --skip-submit \
  --timeout 240 \
  --serial target/oscomp/custom-run/malloc-sparse-pollattr-serial.txt \
  --data target/oscomp/custom-run/malloc-sparse-pollattr-data \
  --submit target/oscomp/custom-run/malloc-vm-submit \
  --build-dir target/oscomp/custom-run/build-malloc-sparse-pollattr \
  --fault-decode
```

Artifacts:

- `target/oscomp/custom-run/malloc-sparse-pollattr-serial.txt`
- `target/oscomp/custom-run/malloc-sparse-pollattr.txtrace`
- `target/oscomp/custom-run/malloc-sparse-pollattr-analyze.txt`
- `target/observe-analyze/malloc-sparse-pollattr.ndjson`
- `target/oscomp/custom-run/malloc-vm-pollattr-names.json`

The trace validates as `1 hart, 16384 slots/hart, 16384 records, 0 framing
errors`; fault-decode again found no trap lines. The instrumented benchmark time
was `14.891245s`, which should be treated as attribution-only because the new
poll markers materially increase trace traffic.

The retained window contained `33` closed poll intervals and one unclosed poll.
For the hot VM pairs, the slow instances overwhelmingly stayed inside one
reactor poll:

| Pair | n | p50 | p95 | max | Classification |
| --- | ---: | ---: | ---: | ---: | --- |
| `fault.resolve.phase 0->1` | 57 | 25us | 44us | 59us | same-poll 54, unknown 3 |
| `fault.publish.phase 0->1` | 57 | 12us | 33us | 42us | same-poll 54, unknown 3 |
| `fault.publish.phase 2->3` | 57 | 55us | 168us | 23.532ms | same-poll 54, unknown 3 |
| `private_set.install.phase 2->3` | 369 | 48us | 163us | 4.793ms | same-poll 338, unknown 31 |
| `fault.publish.phase 3->4` | 57 | 93us | 305us | 4.350ms | same-poll 54, unknown 3 |
| `pmap.publish_batch.insert.phase 1->2` | 314 | 8us | 17us | 4.705ms | same-poll 299, unknown 15 |

No `debug.vm.fault.resolve.wait`, `debug.vm.fault.publish.wait`,
`debug.vm.fault.script.wait`, or `debug.vm.fault.materialize.wait` markers
appeared in this retained window. That makes the current evidence favor
synchronous private-set / pmap / publication-check work for the residual
private-anon outliers, not scheduler pick latency or RangeLock contention.

The one scheduler-sensitive clue still present is broader and separate: the
trace has a `22.894ms` runnable-to-pick delay for task 1, and earlier
edge-reschedule fixes improved fault-heavy malloc cases. The local conclusion is
therefore narrow: the specific residual install/publish outliers sampled here
are same-poll work; the earlier speedup likely came from scheduler edges around
fault-loop/trap-loop boundaries or other async waits outside these hot pairs.

Next implementation lever: reduce synchronous per-page mutation cost. Start with
private-anon monotonic-growth specialization or batching: avoid path-copying
persistent-treap allocation on every ordinary heap fault, and publish contiguous
prefault pages to the private set and pmap resident map as a range where the
contracts permit it. Keep RangeLock wait instrumentation in place for future
multi-task contention runs, but it is not the first lever for this malloc-sparse
sample.

## Size-correlation follow-up

Added publication-boundary size counters:

- `debug.vm.fault.publish.pmap_mapped_pages`
- `debug.vm.fault.publish.private_len`

The follow-up run used the same `malloc-sparse` bracket, saved at:

- `target/oscomp/custom-run/malloc-sparse-sizeattr-serial.txt`
- `target/oscomp/custom-run/malloc-sparse-sizeattr.txtrace`
- `target/oscomp/custom-run/malloc-sparse-sizeattr-analyze.txt`
- `target/observe-analyze/malloc-sparse-sizeattr.ndjson`
- `target/oscomp/custom-run/malloc-vm-sizeattr-names.json`

The trace validates as `1 hart, 16384 slots/hart, 16384 records, 0 framing
errors`. The instrumented benchmark time was `11.000302s`; as above, treat it as
attribution-only.

The analyzer now reports total time per phase pair. In the retained window:

| Pair | n | total | avg | p50 | p95 | max |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `fault.resolve.phase 0->1` | 57 | 0.982ms | 17.2us | 16us | 25us | 44us |
| `fault.publish.phase 0->1` | 57 | 0.582ms | 10.2us | 9us | 15us | 38us |
| `fault.publish.phase 2->3` | 57 | 3.457ms | 60.6us | 53us | 85us | 270us |
| `private_set.install.phase 2->3` | 368 | 60.536ms | 164.5us | 40us | 108us | 3.820ms |
| `fault.publish.phase 3->4` | 57 | 6.424ms | 112.7us | 51us | 128us | 3.116ms |
| `pmap.publish_batch.insert.phase 1->2` | 314 | 16.094ms | 51.3us | 7us | 11us | 3.642ms |

The `fault.publish 2->3` size buckets do not show a convincing monotonic slope
in this retained sample:

- `pmap_mapped_pages=1-16`: `n=19`, avg `62.5us`, p50 `51us`, max `95us`
- `pmap_mapped_pages=17-64`: `n=17`, avg `49.8us`, p50 `49us`, max `65us`
- `pmap_mapped_pages=65-256`: `n=13`, avg `58.8us`, p50 `59us`, max `70us`
- `pmap_mapped_pages=257-1024`: `n=7`, avg `84.1us`, p50 `54us`, max `270us`

`private_len` buckets follow the same pattern. The one larger `270us`
`publish 2->3` sample occurred at `pmap_mapped_pages=271/private_len=257`, but
the bucket median stayed `54us`, so the current evidence does not yet prove a
latent O(n) publication-check term. A larger retained range or a synthetic
100k-page probe would be needed before dismissing the risk entirely.

The actionable ranking is now:

1. `private_set.install` common-case and tail cost: largest sampled total.
2. `pmap.publish_batch.insert` and `fault.publish 3->4`: smaller typical costs,
   but ms tails still land inside one poll.
3. `fault.publish 2->3`: scope-check passed for the sampled range; monitor on
   larger heaps, but it is not the first optimization target from this trace.

## Resident-store reserve follow-up

A co-occurrence scan of `malloc-sparse-sizeattr` found that ms-scale
`private_set.install` and `pmap.publish_batch.insert` spikes sometimes shared
the same poll, but not always; `publish_batch.insert` was still mechanically
consistent with `Vec` backing-store growth. The batch shape confirmed
fault-around is already active in this retained window: 30 `publish_batch`
calls published 314 pages, averaging 10.47 pages/call, with 18 calls publishing
15 pages.

Implemented a narrow resident-store reserve in `VmPmap`:

- single-page publish reserves one slot before inserting a new resident mapping;
- `publish_new_pages_best_effort` reserves `pages.len()` before the batched
  insert loop;
- `PmapResidentStore` remains the same sorted `Vec` contract.

The post-reserve run used:

- `target/oscomp/custom-run/malloc-sparse-reserveattr-serial.txt`
- `target/oscomp/custom-run/malloc-sparse-reserveattr.txtrace`
- `target/oscomp/custom-run/malloc-sparse-reserveattr-analyze.txt`
- `target/observe-analyze/malloc-sparse-reserveattr.ndjson`
- `target/oscomp/custom-run/malloc-vm-reserveattr-names.json`

The trace validates as `1 hart, 16384 slots/hart, 16384 records, 0 framing
errors`; fault-decode again found no trap lines. The instrumented benchmark time
was `11.312297s`, attribution-only.

Compared with the pre-reserve size-attribution run, the batch shape stayed
identical (`30` calls, average `10.47` pages/call), while the resident-store
insert tail collapsed:

| Pair | Pre-reserve | Post-reserve |
| --- | ---: | ---: |
| `pmap.publish_batch.insert 1->2` total | 16.094ms | 2.635ms |
| `pmap.publish_batch.insert 1->2` avg | 51.3us | 8.4us |
| `pmap.publish_batch.insert 1->2` p50 | 7us | 8us |
| `pmap.publish_batch.insert 1->2` p95 | 11us | 10us |
| `pmap.publish_batch.insert 1->2` max | 3.642ms | 16us |

This confirms the pmap batch-insert residual was a `Vec` capacity-growth tail,
not missing fault-around and not a median per-page cost. Remaining sampled VM
tail is now dominated by `private_set.install` (`68.541ms` total in the
post-reserve retained window, max `6.700ms`) and occasional
`fault.publish 3->4` spikes (`7.837ms` total, max `4.349ms`).

Next lever: investigate and reduce `PrivatePageSet` path-copy allocation tails.
The lowest-risk first check is to add capacity/allocation counters or a
monotonic-growth specialization for private-anon pages; do not spend more time
on pmap resident-store batching until non-append insert patterns are measured in
bubble or a synthetic reuse probe.

## Full-run private-set aggregate follow-up

Added low-overhead aggregate phase counters for the full observe bracket:

- `PrivatePageSet::install_if_absent` core insert time (`phase 2 -> 3`
  equivalent, measured around `insert_if_absent` itself);
- `pmap.publish_batch.insert` core resident-store insert time.

The counters reset on `tx_observe_begin` / `tx_observe_trace_on` and print as
serial text from a new `tx_observe` pre-dump hook immediately before
`TXTRACE-BEGIN`, so the totals are not lost when the 16,384-record observe ring
fills. The first run used a stale submit kernel; after refreshing
`target/oscomp/custom-run/malloc-vm-submit/kernel-rv` with
`cargo xtask oscomp submit --target rv64-qemu --submit target/oscomp/custom-run/malloc-vm-submit`,
the focused `malloc-sparse` run was saved at:

- `target/oscomp/custom-run/malloc-sparse-fulltotals2-serial.txt`
- `target/oscomp/custom-run/malloc-sparse-fulltotals2.txtrace`
- `target/oscomp/custom-run/malloc-sparse-fulltotals2-analyze.txt`
- `target/observe-analyze/malloc-sparse-fulltotals2.ndjson`
- `target/oscomp/custom-run/malloc-vm-fulltotals-names.json`

Guest result:

```text
b_malloc_sparse (0)
  time: 12.348792000, virt: 0, res: 0, dirty: 0

:vm:phase-total:private_set.install count=10023 total_ns=3041188000 avg_ns=303420 max_ns=16575000 touched_total=87497 touched_max=20 len_max=4063
:vm:phase-total:pmap.publish_batch.insert count=9337 total_ns=56857000 avg_ns=6089 max_ns=1359000
```

This closes the attribution caveat from the retained-ring samples:
`private_set.install` is a full-run cost, about `3.041s` of the `12.349s`
instrumented `malloc-sparse` bracket. The pmap resident-store insert path is now
about `56.9ms`, roughly `1.9%` of the private-set insert total. The retained
trace still shows pmap insert flat after the reserve (`314` retained inserts,
`3.486ms` total, `33us` max), while retained `private_set.install` remains
tail-heavy (`64.733ms` over `368`, max `3.957ms`). Fault-decode again found no
trap lines.

Current conclusion: the remaining VM malloc lever is `PrivatePageSet`, not pmap
resident-store publication. Because the set maps per-VA offsets to distinct
private frames, range compression is not appropriate for the authoritative
payload. The next implementation decision is between a sorted-Vec/private-page
store with reserve for the monotonic private-anon path, or an in-place treap
mutation design that defers path-copying to fork/snapshot time while preserving
reader semantics.

## Slab allocator private-set tail follow-up

The full-run install distribution then separated algorithmic growth from
allocation tails. The focused `malloc-sparse` run saved at
`target/oscomp/custom-run/malloc-sparse-install-dist-serial.txt` recorded all
`10023` private installs with no sample drops:

```text
:vm:phase-total:private_set.install count=10023 total_ns=2930970000 avg_ns=292424 max_ns=40437000 touched_total=87497 touched_max=20 len_max=4063 sample_count=10023 sample_dropped=0
:vm:phase-percentile:private_set.install sample_count=10023 p50_ns=52000 p95_ns=183000 p99_ns=7673000 max_ns=40437000
:vm:phase-total:pmap.publish_batch.insert count=9337 total_ns=55342000 avg_ns=5927 max_ns=91000
```

Bucket medians rose only mildly with resident count (`31us` at `0-16`,
`56us` at `1025-4096`), while p99/max were millisecond-scale. That supports an
allocation-refill tail rather than treap degeneration or an O(n) median slope.

Implemented two bounded substrate-slab policy changes:

- retain one fully empty page per small size class for immediate reuse;
- refill medium small-allocation classes in page batches
  (`64B -> 4 pages`, `128B -> 8 pages`, `256B -> 4 pages`, `512B -> 2 pages`),
  falling back to a single page if the contiguous refill is unavailable.

The retain-only probe was saved at:

- `target/oscomp/custom-run/malloc-sparse-slab-retain-serial.txt`
- `target/oscomp/custom-run/malloc-sparse-slab-retain.txtrace`
- `target/oscomp/custom-run/malloc-sparse-slab-retain-analyze.txt`
- `target/observe-analyze/malloc-sparse-slab-retain.ndjson`
- `target/oscomp/custom-run/malloc-vm-slab-retain-names.json`

Retaining one empty slab page helped but did not collapse the tail:

```text
b_malloc_sparse (0)
  time: 9.765496000, virt: 0, res: 0, dirty: 0

:vm:phase-total:private_set.install count=10023 total_ns=2547107000 avg_ns=254126 max_ns=22726000
:vm:phase-percentile:private_set.install sample_count=10023 p50_ns=50000 p95_ns=197000 p99_ns=6986000 max_ns=22726000
```

Adding batched medium-class refills was the larger win. The run was saved at:

- `target/oscomp/custom-run/malloc-sparse-slab-batch-serial.txt`
- `target/oscomp/custom-run/malloc-sparse-slab-batch.txtrace`
- `target/oscomp/custom-run/malloc-sparse-slab-batch-analyze.txt`
- `target/observe-analyze/malloc-sparse-slab-batch.ndjson`
- `target/oscomp/custom-run/malloc-vm-slab-batch-names.json`

Guest result:

```text
b_malloc_sparse (0)
  time: 5.892056000, virt: 0, res: 0, dirty: 0

:vm:phase-total:private_set.install count=10023 total_ns=1015781000 avg_ns=101345 max_ns=10022000 touched_total=87497 touched_max=20 len_max=4063 sample_count=10023 sample_dropped=0
:vm:phase-percentile:private_set.install sample_count=10023 p50_ns=47000 p95_ns=97000 p99_ns=220000 max_ns=10022000
:vm:phase-total:pmap.publish_batch.insert count=9337 total_ns=47723000 avg_ns=5111 max_ns=69000
```

The trace validates as `1 hart, 16384 slots/hart, 16384 records, 0 framing
errors`; fault-decode again found no trap lines. Retained trace window:
`private_set.install 2->3` was `32.444ms` over `368` samples (`p50=47us`,
`p95=122us`, `max=4.075ms`), and `pmap.publish_batch.insert 1->2` stayed flat
at `3.744ms` over `314` samples (`p50=10us`, `p95=17us`, `max=103us`).

Conclusion: the full-run p50/p95 data did not justify replacing
`PrivatePageSet` yet. The dominant tail was slab refill behavior from
path-copy allocation churn; bounded slab reuse/batched refill reduced
`private_set.install` total from `2.931s` to `1.016s` and moved p99 from
`7.673ms` to `220us`. Remaining work is to investigate the rare 8-10ms refill
maxes or reduce path-copy allocation count structurally, but that is now a
second-order lever relative to the previous VM blocker.

An independent-page refill experiment tested whether the remaining rare maxes
were caused by the contiguous refill requirement. The patch changed medium
class refill from one N-page contiguous `reserve_run` to up to N independent
single-page reserves, then was reverted after measurement. The run was saved at:

- `target/oscomp/custom-run/malloc-sparse-slab-independent-serial.txt`
- `target/oscomp/custom-run/malloc-sparse-slab-independent-run.log`

Result:

```text
b_malloc_sparse (0)
  time: 10.706895000, virt: 0, res: 0, dirty: 0

:vm:phase-total:private_set.install count=10023 total_ns=2842332000 avg_ns=283580 max_ns=55272000 touched_total=87497 touched_max=20 len_max=4063 sample_count=10023 sample_dropped=0
:vm:phase-percentile:private_set.install sample_count=10023 p50_ns=51000 p95_ns=115000 p99_ns=314000 max_ns=55272000
```

This falsifies the independent-page refill idea for the current bitmap
allocator: p50/p95 stayed acceptable, but total and max regressed badly. The
probable mechanism is that one contiguous N-page run costs one allocator search
and one run commit, while N independent pages require N reserve/commit cycles
under the slab refill path. Keep the contiguous batched refill until the page
allocator grows a true bulk-noncontiguous reservation API.

With the private-set tail reduced, the retained `malloc-sparse-slab-batch`
window shows the next visible work elsewhere:

- `sys_rt_sigprocmask`: `36.873ms` over six spans, including one `34.267ms`
  outlier;
- `sys_mmap`: `30.818ms` over 33 spans;
- `sys_clone`: `14.553ms` for the benchmark child;
- largest inter-record gaps are now PageBacked/file-backed fault-step gaps
  (`debug.pagebacked.fault_step.kind=2 -> done=2`, up to `94.057ms`) and
  `debug.vm.user.pagebacked.phase=0 -> 1` (`31.174ms`).

Those PageBacked gaps are likely child setup or `/proc/self/smaps` tail rather
than the allocator body, but they are now larger than the residual private-set
sampled max. The next whole-run attribution pass should add full-run totals and
percentiles around PageBacked fault-step, `sys_rt_sigprocmask`, and `mmap`
commit/setup before further private-set structural work.

## Body-only bracket follow-up

The broad `--observe-bracket` wraps selected libc-bench `RUN()` calls in
`main()`, so it includes child setup before `run_bench()` starts timing and the
`/proc/self/smaps` tail after the benchmark body. That made large
PageBacked/file-backed gaps and `rt_sigprocmask` outliers visible in the trace,
but not necessarily relevant to the score number.

Added `--observe-body-bracket` to `tools/oscomp-custom-run.py` for the single
benchmark case where we need body-only attribution. It patches the selected
`run_bench()` sequence as:

```c
clock_gettime(CLOCK_REALTIME, &tv0);
syscall(334);
bench(params);
syscall(335);
print_stats(tv0);
```

`--observe-body-bracket` is mutually exclusive with `--observe-threshold` and
`--observe-bracket`, and it requires a single `--libcbench-only` selector. The
Python harness test suite pins both constraints and the placement.

The valid corrected run required rebuilding the RV64 kernel and refreshing
`target/oscomp/custom-run/malloc-vm-submit/kernel-rv`; an earlier attempt used
a stale submit from the rejected independent-page refill experiment and is not
part of the evidence set. The corrected command shape was:

```sh
python3 tools/oscomp-custom-run.py \
  --libcbench \
  --libcbench-only malloc-sparse \
  --observe-body-bracket \
  --run \
  --skip-build \
  --skip-submit \
  --timeout 240 \
  --serial target/oscomp/custom-run/malloc-sparse-bodyonly-batch-serial.txt \
  --data target/oscomp/custom-run/malloc-sparse-bodyonly-batch-data \
  --submit target/oscomp/custom-run/malloc-vm-submit \
  --build-dir target/oscomp/custom-run/build-malloc-sparse-bodyonly-batch \
  --fault-decode
```

Artifacts:

- `target/oscomp/custom-run/malloc-sparse-bodyonly-batch-serial.txt`
- `target/oscomp/custom-run/malloc-sparse-bodyonly-batch-run.log`
- `target/oscomp/custom-run/malloc-sparse-bodyonly-batch.txtrace`
- `target/oscomp/custom-run/malloc-sparse-bodyonly-batch-analyze.txt`
- `target/observe-analyze/malloc-sparse-bodyonly-batch.ndjson`
- `target/oscomp/custom-run/malloc-vm-bodyonly-batch-names.json`

Because `tx_trace_off` requests the dump/poweroff at the next kernel boundary,
the normal libc-bench `time:` line is not printed in this mode. That is
expected: the purpose is the pre-dump body aggregate, not a scored run.

Full body aggregates:

```text
:vm:phase-total:private_set.install count=10023 total_ns=1045909000 avg_ns=104350 max_ns=10543000 touched_total=87497 touched_max=20 len_max=4063 sample_count=10023 sample_dropped=0
:vm:phase-percentile:private_set.install sample_count=10023 p50_ns=48000 p95_ns=103000 p99_ns=268000 max_ns=10543000
:vm:phase-total:pmap.publish_batch.insert count=9337 total_ns=50337000 avg_ns=5391 max_ns=76000
```

The trace validates as `1 hart, 16384 slots/hart, 16384 records, 0 framing
errors`, and fault-decode found no trap lines. The retained body-only window is
`257.424ms` and includes only `sys_mmap` and `sys_brk` syscall spans:

| Span | n | total | avg | max |
| --- | ---: | ---: | ---: | ---: |
| `sys_mmap` | 34 | 30.124ms | 886us | 4.946ms |
| `sys_brk` | 2 | 1.491ms | 745.5us | 934us |

No `rt_sigprocmask`, `clone`, or futex spans appear in the retained body-only
window. The largest remaining non-anon gap is one PageBacked fault-step pair
(`debug.pagebacked.fault_step.kind=2 -> done=2`) at `13.343ms`, much smaller
than the earlier broad-bracket `94.057ms` PageBacked gap and no longer mixed
with smaps-tail-heavy spans.

Current body-only VM read:

1. `PrivatePageSet` install remains a real body cost at about `1.046s` over
   10,023 installs after the slab refill fix.
2. `pmap.publish_batch.insert` is stable at about `50.337ms` full-body total.
3. Raw `mmap`/`brk` spans are visible but much smaller than private-anon fault
   insertion, and setup-only `rt_sigprocmask` / clone spans should not drive the
   next score optimization.
4. PageBacked cold file-fault gaps are a separate wall-time/setup axis unless a
   body-only trace shows they dominate a timed benchmark body.

Next attribution target: add body-only full-run totals/percentiles for the
remaining private-anon phases around frame allocation/zeroing and pmap publish,
then rank those against the now-reduced `PrivatePageSet` cost. Keep PageBacked
file-fault and `smaps` work in a separate report unless the optimization target
is total wall time rather than libc-bench body time.

## Fault-around multiplier check

The body-only aggregate originally printed:

```text
:vm:phase-total:private_set.install count=10023 ... touched_total=87497 touched_max=20 ...
:vm:phase-total:pmap.publish_batch.insert count=9337 ...
```

`touched_total` is not a count of pages installed by fault-around. It is the
number of treap nodes touched by `PrivatePageTree::insert_if_absent` while
path-copying the private-page tree. The relevant page counts are:

- `private_set.install count`: total private pages installed, including the
  leading fault page and any prefault tail pages.
- `pmap.publish_batch.insert count`: tail pages successfully published by the
  speculative prefault batch path.

To make that distinction explicit in future dumps, the debug output now prints
`treap_touched_total` / `treap_touched_max` and a derived
`:vm:phase-total:private_anon.prefault` line. The refreshed body-only run used
the same command shape as above and saved:

- `target/oscomp/custom-run/malloc-sparse-bodyonly-prefault-serial.txt`
- `target/oscomp/custom-run/malloc-sparse-bodyonly-prefault-run.log`
- `target/oscomp/custom-run/malloc-sparse-bodyonly-prefault.txtrace`
- `target/oscomp/custom-run/malloc-sparse-bodyonly-prefault-analyze.txt`
- `target/observe-analyze/malloc-sparse-bodyonly-prefault.ndjson`
- `target/oscomp/custom-run/malloc-vm-bodyonly-prefault-names.json`

Key dump lines:

```text
:vm:phase-total:private_set.install count=10023 total_ns=1194202000 avg_ns=119146 max_ns=15868000 treap_touched_total=87497 treap_touched_max=20 len_max=4063 sample_count=10023 sample_dropped=0
:vm:phase-percentile:private_set.install sample_count=10023 p50_ns=52000 p95_ns=115000 p99_ns=308000 max_ns=15868000
:vm:phase-total:pmap.publish_batch.insert count=9337 total_ns=49932000 avg_ns=5347 max_ns=496000
:vm:phase-total:private_anon.prefault private_installs=10023 batch_pmap_publishes=9337 non_batch_private_installs=686
```

This falsifies the proposed 8x wasted-prefault multiplier for
`malloc-sparse`. The run installed about the expected number of private pages
for 10,000 near-page-sized allocations, and `9337` of those pages came through
the batch-prefault publication path. The non-batch leading publication count is
only `686`, which is in line with a 16-page sequential fault-around window
rather than one kernel fault per allocation.

The refreshed retained body-only trace validates as `1 hart, 16384 slots/hart,
16384 records, 0 framing errors`; fault-decode found no trap lines. It still
has only `sys_mmap` and `sys_brk` syscall spans (`mmap` `30.278ms`, `brk`
`1.434ms`), and the analyzer reports no futex operations.

Conclusion: do not shrink the private-anon fault-around window for this
benchmark. The remaining body optimization problem is per-installed-page cost:
private-frame allocation/zeroing, private-tree insertion, pmap reservation /
commit, and publication bookkeeping.

## Treap node-allocation follow-up

Added a direct allocation counter to the immutable private-tree insert path.
This counts new `PrivatePageNode` allocations created by
`PrivatePageTree::insert_if_absent`, including extra nodes allocated by treap
rotations. It intentionally does not count the one new `Arc<PrivateFrame>` per
installed page, since an in-place private-page store would still need one
authoritative frame entry for each resident private page.

The refreshed body-only run saved:

- `target/oscomp/custom-run/malloc-sparse-bodyonly-nodealloc-serial.txt`
- `target/oscomp/custom-run/malloc-sparse-bodyonly-nodealloc-run.log`
- `target/oscomp/custom-run/malloc-sparse-bodyonly-nodealloc.txtrace`
- `target/oscomp/custom-run/malloc-sparse-bodyonly-nodealloc-analyze.txt`
- `target/observe-analyze/malloc-sparse-bodyonly-nodealloc.ndjson`
- `target/oscomp/custom-run/malloc-vm-bodyonly-nodealloc-names.json`

Key dump lines:

```text
:vm:phase-total:private_set.install count=10023 total_ns=1340305000 avg_ns=133722 max_ns=17547000 treap_touched_total=87497 treap_touched_max=20 treap_node_alloc_total=107459 treap_node_alloc_max=48 len_max=4063 sample_count=10023 sample_dropped=0
:vm:phase-percentile:private_set.install sample_count=10023 p50_ns=60000 p95_ns=158000 p99_ns=460000 max_ns=17547000
:vm:phase-total:pmap.publish_batch.insert count=9337 total_ns=62806000 avg_ns=6726 max_ns=190000
:vm:phase-total:private_anon.prefault private_installs=10023 batch_pmap_publishes=9337 non_batch_private_installs=686
```

Interpretation:

- `treap_touched_total / count = 8.73` touched nodes per install.
- `treap_node_alloc_total / count = 10.72` immutable treap-node allocations per
  install.
- `treap_node_alloc_max=48` shows rotations can allocate substantially more
  nodes than the root-to-leaf touched count for individual inserts.
- The prefault split is unchanged: the 16-page private-anon window is still
  suppressing leading faults, not inflating installed pages.

The trace validates as `1 hart, 16384 slots/hart, 16384 records, 0 framing
errors`; fault-decode found no trap lines. The retained body window still has
only `sys_mmap` and `sys_brk` syscall spans, and the analyzer reports no futex
operations.

This confirms the next reducible install lever: remove immutable-node churn from
ordinary private-anon installs. An in-place tree or sorted resident store would
still traverse/search and allocate one entry per page, but it should avoid the
~10.7 treap-node allocations per page. The tradeoff is the known one: the
current persistent tree buys immutable-reader behavior and cheap fork/snapshot;
an in-place design needs a replacement reader/fork scheme before it is safe on
SMP and fork-heavy paths. The absolute timing in this run is attribution-only,
because adding a per-install node-allocation counter and trace counter increases
debug overhead; use the allocation counts and distribution, not the `1.340s`,
as the decision input.
