# 2026-06-04 Wholesale libcbench choke-point observe pass

## Scope

The old observe/custom-run artifacts under `target/oscomp/custom-run` were
trimmed before this pass. The cleanup retained the previous narrow pthread
reference
`target/oscomp/custom-run/recipe-bplus-narrow2-pthread-smp1-20260603-continued`
and the fresh runs listed below; the tree was reduced to about `1.7G` after the
new captures.

Fresh capture artifacts:

- `target/oscomp/custom-run/whole-suite-chokepoints-20260604`
  - Intended as a whole-suite choke-point run, but the wrapper defaulted to
    `--test pthread`, so this is pthread-only.
  - Runtime quality: `complete=true`, `raw_records=2625927`,
    `lost=0`, `overwritten=0`, `repairs=0`.
- `target/oscomp/custom-run/whole-suite-mm-io-pthread-20260604`
  - Explicit `--test mm-io-pthread`; covers malloc, stdio, and pthread.
  - Runtime quality: `complete=true`, `raw_records=4465780`,
    `lost=0`, `overwritten=0`, `repairs=0`.
- `target/oscomp/custom-run/whole-suite-regex-20260604`
  - Explicit `--test regex`.
  - Runtime quality: `complete=true`, `raw_records=232221`,
    `lost=0`, `overwritten=0`, `repairs=0`.

Build flags for the fresh captures were:

```sh
RUSTFLAGS="--cfg tx_lock_metrics --cfg tx_lock_metrics_vm --cfg tx_lock_metrics_fs --cfg tx_vm_recipe_bplus"
```

The captures are live-observe attribution runs, not clean no-observe score
runs. They intentionally keep the current narrow profile: `spans`, `counters`,
and `lock_rows` are populated, while `allocation_rows`, `ds_method_rows`, and
`sched_intervals` are empty in these artifacts.

## Benchmark shape

The mixed `mm-io-pthread` serial timings rank the lanes as:

| lane | summed timed body |
| --- | ---: |
| pthread | `106.356s` |
| stdio | `28.348s` |
| malloc | `27.523s` |

Per-test timings in that mixed run:

| test | time |
| --- | ---: |
| `b_pthread_createjoin_serial1` | `24.273002s` |
| `b_pthread_createjoin_serial2` | `21.774764s` |
| `b_pthread_create_serial1` | `19.957685s` |
| `b_pthread_createjoin_minimal1` | `20.954345s` |
| `b_pthread_createjoin_minimal2` | `19.284602s` |
| `b_stdio_putcgetc` | `14.122992s` |
| `b_stdio_putcgetc_unlocked` | `14.224688s` |
| `b_malloc_big1` | `8.746020s` |
| `b_malloc_big2` | `6.324657s` |
| `b_malloc_bubble` | `5.742705s` |
| `b_malloc_sparse` | `4.948124s` |

The regex run is much smaller in this profile:

| test | time |
| --- | ---: |
| `b_regex_compile` | `9.149745s` |
| `b_regex_search` | `0.138589s` |
| `b_regex_search` | `0.213628s` |

Conclusion: pthread lifecycle remains the score-dominant lane in the current
mixed window. Stdio and malloc are close enough that either is a reasonable
second lane, but regex is not first unless a score weights it unusually.

## Syscall spans

`sys_wait4` is the largest raw span in every run, but it is parent blocked time
while child benchmarks run. It is not counted below as a kernel service
choke point.

Top mixed-run syscall/service spans after excluding `wait4`:

| span | n | total | p50 | p99 | max |
| --- | ---: | ---: | ---: | ---: | ---: |
| `sys_clone` | `12521` | `14.781s` | `1044us` | `4509us` | `238955us` |
| `sys_munmap` | `11094` | `11.696s` | `610us` | `15804us` | `64286us` |
| `sys_mmap` | `13929` | `9.126s` | `549us` | `1942us` | `20622us` |
| `sys_writev` | `9790` | `8.812s` | `742us` | `7973us` | `25355us` |
| `sys_rt_sigprocmask` | `50124` | `8.014s` | `151us` | `301us` | `27960us` |
| `sys_read` | `9766` | `5.535s` | `520us` | `1197us` | `39250us` |
| `sys_mprotect` | `7511` | `5.432s` | `632us` | `4372us` | `7770us` |
| `sys_futex` | `17742` | `4.895s` | `117us` | `586us` | `709110us` |
| `sys_exit` | `12505` | `4.396s` | `339us` | `582us` | `4268us` |

The pthread-only capture gives the same ranking, with a larger pthread body:

| span | n | total | p50 | p99 | max |
| --- | ---: | ---: | ---: | ---: | ---: |
| `sys_clone` | `12507` | `18.843s` | `1289us` | `5250us` | `124679us` |
| `sys_munmap` | `10001` | `10.337s` | `733us` | `5037us` | `49108us` |
| `sys_mmap` | `12501` | `9.907s` | `715us` | `2206us` | `12933us` |
| `sys_rt_sigprocmask` | `50046` | `9.905s` | `164us` | `706us` | `27575us` |
| `sys_futex` | `18027` | `6.703s` | `145us` | `3841us` | `141529us` |
| `sys_mprotect` | `7501` | `6.566s` | `708us` | `4771us` | `24746us` |
| `sys_exit` | `12501` | `5.653s` | `390us` | `1379us` | `51477us` |

Conclusion: the pthread lane is not primarily join lock contention in this
capture. It is the lifecycle spine: clone, mmap, mprotect, munmap,
sigprocmask, futex wait/parking, and exit.

## Lock service rows

Lock wait is negligible relative to service in this profile, so the lock rows
mostly measure work inside critical sections rather than queueing.

Resolved lock IDs:

| lock id | name |
| ---: | --- |
| `3683638860` | `debug.lock.vm.recipe_index.mutation` |
| `243366756` | `debug.lock.vm.private_page_set.pages` |
| `436464615` | `debug.lock.vm.pmap.state` |
| `2378190548` | `debug.lock.vm.range_lock.state` |
| `3595587589` | `debug.lock.vm.recipe_reclaim.deferred` |
| `2524062173` | `debug.lock.fs.tmpfs.state` |

Mixed-run lock service:

| lock | n | service total | avg | p50 | p99 | wait total |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `recipe_index.mutation` | `32588` | `12.026s` | `369us` | `273us` | `1397us` | `0.061s` |
| `private_page_set.pages` | `142990` | `8.020s` | `56us` | `11us` | `96us` | `0.255s` |
| `pmap.state` | `320408` | `7.319s` | `23us` | `10us` | `150us` | `0.588s` |
| `range_lock.state` | `152470` | `1.850s` | `12us` | `10us` | `28us` | `0.297s` |
| `recipe_reclaim.deferred` | `76051` | `0.527s` | `7us` | `6us` | `14us` | `0.137s` |
| `tmpfs.state` | `16` | `0.007s` | `444us` | `35us` | `4773us` | `0.000079s` |

Pthread-only lock service:

| lock | n | service total | avg | p50 | p99 | wait total |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `recipe_index.mutation` | `30015` | `13.727s` | `457us` | `331us` | `4114us` | `0.069s` |
| `pmap.state` | `163643` | `4.162s` | `25us` | `13us` | `169us` | `0.393s` |
| `range_lock.state` | `110486` | `1.852s` | `17us` | `13us` | `76us` | `0.264s` |
| `private_page_set.pages` | `44970` | `1.045s` | `23us` | `12us` | `110us` | `0.103s` |
| `recipe_reclaim.deferred` | `72438` | `0.607s` | `8us` | `6us` | `40us` | `0.158s` |

Conclusion: VM recipe mutation is still the top measured lock-held subsystem.
The private-page and pmap locks are high-frequency next-tier costs in the mixed
run, especially where malloc/stdio drive page faults and user copy. The tmpfs
lock is not a broad stdio explanation here.

## Recipe publish/reclaim

Stable recipe publish counters were present; high-volume allocation and shape
metrics were not enabled. Operation mapping:

- `0`: `MapRequireFree`
- `1`: `MapFixedReplace`
- `2`: `Unmap`
- `3`: `Protect`
- `7`: `ReplaceEntry`

Mixed-run recipe publish:

| op | n | total | avg | p50 | p99 | max | avg touched | avg nodes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `MapRequireFree` | `13941` | `3.899s` | `280us` | `218us` | `671us` | `20206us` | `9.586` | `1.964` |
| `Unmap` | `11094` | `3.679s` | `332us` | `199us` | `893us` | `25862us` | `7.094` | `1.285` |
| `Protect` | `7511` | `3.506s` | `467us` | `379us` | `4119us` | `7522us` | `10.562` | `2.265` |

Pthread-only recipe publish:

| op | n | total | avg | p50 | p99 | max | avg touched | avg nodes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `Unmap` | `10001` | `4.750s` | `475us` | `269us` | `4342us` | `44894us` | `6.550` | `1.280` |
| `Protect` | `7501` | `4.309s` | `575us` | `436us` | `4363us` | `23506us` | `10.564` | `2.266` |
| `MapRequireFree` | `12501` | `3.612s` | `289us` | `255us` | `866us` | `7981us` | `8.903` | `1.956` |

Recipe reclaim is not the dominant part of this capture:

| run | n | total | avg | p50 | p99 | max |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| mixed | `32542` | `0.887s` | `27us` | `22us` | `72us` | `7727us` |
| pthread-only | `29969` | `1.084s` | `36us` | `27us` | `164us` | `2084us` |
| regex | `4113` | `0.086s` | `21us` | `13us` | `61us` | `156us` |

Conclusion: the current B+ recipe index is not being sunk by reclaim in this
profile. The remaining measured VM recipe work is publish under the mutation
lock, with `Unmap`, `Protect`, and `MapRequireFree` all large enough that a
single-op microfix is unlikely to move the full pthread lane alone.

## Stdio and regex

Stdio:

- `b_stdio_putcgetc=14.122992s` and
  `b_stdio_putcgetc_unlocked=14.224688s`; locked and unlocked are effectively
  tied in this run.
- Mixed-run I/O syscalls are `sys_writev=8.812s` and `sys_read=5.535s`.
- The `tmpfs.state` lock has only `16` acquisitions and `0.007s` service, so
  the current stdio lane is not explained by tmpfs lock contention.

Next stdio evidence should enable a targeted FS/user-copy profile rather than
turning broad scheduler or VM shape metrics back on. The likely question is
still read/write/user-copy path cost, not FILE locking.

Regex:

- `b_regex_compile=9.149745s`, but syscall-visible kernel work in the regex
  capture is mainly `sys_mmap=1.980s` and `sys_munmap=1.757s`.
- The rest is likely userspace regex compile/QEMU compute unless a narrower
  trace proves allocator or VM traffic inside the timed body dominates.

Regex should not outrank pthread or stdio/VM from this evidence.

## Current ranking

1. **Pthread lifecycle spine.** This is the score-dominant lane in the mixed
   capture (`106.356s`). The actionable kernel surface is the lifecycle VM
   path and process/thread syscall spine: `clone`, `mmap`, `mprotect`,
   `munmap`, `rt_sigprocmask`, `futex`, and `exit`.
2. **VM recipe publish under `recipe_index.mutation`.** This remains the
   largest named lock-held section (`12.026s` mixed, `13.727s` pthread-only)
   with negligible wait. `Unmap`, `Protect`, and `MapRequireFree` are all
   material.
3. **Pmap/private-page/range-lock high-frequency VM support costs.** In the
   mixed run, `private_page_set.pages` and `pmap.state` together account for
   about `15.339s` of service with low per-call medians. This is more visible
   outside pthread-only, so it likely belongs to malloc/page-fault/user-copy
   pressure.
4. **Stdio read/write path.** `writev+read` total `14.347s`; tmpfs locking is
   not the explanation in this profile, and locked/unlocked stdio timings are
   the same.
5. **Malloc/VM bodies.** Still meaningful (`27.523s` lane sum), but below
   pthread and roughly tied with stdio. Needs a lane-specific body trace if the
   next goal is malloc score rather than whole-suite choke points.
6. **Regex compile.** Visible as a benchmark time, but not yet a kernel
   choke point from the syscall/lock evidence.

## Caveats

- The first fresh run named `whole-suite-chokepoints-20260604` is pthread-only
  because `tools/oscomp-observe-live.py` defaults to `--test pthread`.
- The live ring was complete and lossless in all three fresh captures, but most
  records were produced by hart 0. Treat the results as valid for these
  workloads, not as evidence that AP-local probes are evenly represented.
- `allocation_rows`, `ds_method_rows`, and `sched_intervals` are empty in this
  profile. A scheduler, allocator, or DS-policy ranking requires a narrower
  follow-up with those gated families explicitly enabled.
- Current `names.json` did not include several hashed lock/counter labels.
  Lock names in this note were resolved by hashing the lock literals in source.
- The branch still has unrelated dirty VFS changes; this observe pass did not
  modify those files.

## Next pass

For score work, run one focused pthread lifecycle observe with:

- current narrow lock/publish counters,
- scheduler wake/pick metrics enabled only if investigating futex/parking
  tails,
- VM pmap/private-page attribution enabled only if investigating
  `mmap`/`mprotect`/`munmap` internals,
- allocator/DS rows enabled only for one lane at a time.

For stdio work, run a focused `stdio` capture with FS read/write/user-copy
markers, not broad VM recipe shape metrics.
