# B+ Recipe Index Percentile Follow-Up

Date: 2026-06-02

## Context

The persistent chunked B+ recipe index landed in commit `4b8a1f40`
(`vm: add persistent B+ recipe index`).  This note records the post-commit
test and the latest completed percentile comparison so future work does not
read only the headline recipe-index critical-section average.

## Verification

- `RUSTFLAGS="--cfg tx_vm_recipe_bplus" cargo test -p tx-subsystems bplus_ -- --nocapture`
  passed all 18 matched B+ tests.
- `cargo xtask progress validate` passed with 27 progress records.
- No `qemu-system-riscv64`, `tx-trace-daemon`, `live-guest-mem`, or
  `oscomp-observe-live` processes were left running after the failed fresh
  capture attempt.

## Artifacts

- Treap reference:
  `target/oscomp/custom-run/recipe-protect-pthread-delayed-measure-20260601-222957`
  (`complete=false`, `raw_records=7398077`, `lost_records=14401`,
  `overwritten_records=0`, `repairs=0`).
- B+ comparable completed run:
  `target/oscomp/custom-run/recipe-bplus-parentcopy-pthread-lock-20260602-151000`
  (`complete=true`, `raw_records=7461908`, `lost_records=0`,
  `overwritten_records=0`, `repairs=0`).
- Fresh post-commit capture attempt:
  `target/oscomp/custom-run/recipe-bplus-localrepair-pthread-lock-20260602-commit`
  produced zero raw records after stalling before the observe bracket, so it is
  not usable as performance evidence.

## Percentile Read

Against the treap reference, B+ cuts per-publish node/chunk work sharply but
does not reduce the total critical-section service yet:

- `debug.vm.recipe.publish.node_allocs`: avg `8.213 -> 1.808`, p50 `7 -> 1`,
  p99 `23 -> 5`, max `38 -> 7`.
- `debug.lock.vm.recipe_index.mutation service`: avg `645.529us -> 716.971us`,
  p50 `534us -> 595us`, p95 `1.115ms -> 1.216ms`, p99 `4.302ms -> 4.749ms`.
- `debug.vm.recipe.publish.duration_ns`: avg `528.988us -> 597.440us`,
  p50 `423us -> 473us`, p99 `4.191ms -> 4.634ms`.
- `debug.vm.recipe.reclaim_tree.nodes`: avg `433.636 -> 54.286`, p99
  `4704 -> 587`, but reclaim duration regressed from avg `73.521us` to
  `156.492us`.

Syscall-level movement in the same comparison shows the B+ run still slightly
slower in the pthread lifecycle VM spine:

- `sys_clone`: avg `1.922ms -> 2.184ms`, p99 `6.134ms -> 7.180ms`.
- `sys_mmap`: avg `1.088ms -> 1.188ms`, p99 improved `2.942ms -> 2.249ms`.
- `sys_munmap`: avg `1.295ms -> 1.410ms`, p99 `5.114ms -> 5.882ms`.
- `sys_mprotect`: avg `1.242ms -> 1.332ms`, p99 roughly flat
  `5.656ms -> 5.725ms`.
- Futex/wait spans improved in the B+ run, but that is not enough to offset
  recipe publish/reclaim and VM spine costs.

## Interpretation

B+ achieved the structural target of reducing persistent-tree allocation count,
but the reduced node count is being converted into extra chunk copy/reclaim
cost rather than lower service time.  The next B+ optimization should target
chunk build/drop cost and reclaim locality, not more tree-depth work.  Do not
promote B+ as default until a clean pthread SMP4 observe run beats the treap
service baseline and the VM malloc regression guard is rechecked.
