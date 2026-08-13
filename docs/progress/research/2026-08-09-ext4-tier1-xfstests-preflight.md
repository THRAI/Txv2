# Ext4 Tier 1 xfstests Docker Preflight

**Date:** 2026-08-09

## Finding

The manifest-pinned xfstests source does not ship a generated `configure`
script. Its top-level `Makefile` defines `make configure` as the autotools
generation step, so invoking `./configure` directly failed before any selected
helper could build. The subsequent top-level `make ltp/fsstress` shape was also
wrong: it selected GNU make's implicit C rule instead of `ltp/Makefile`, which
owns the helper and depends on `lib/libtest.la`.

## Change

`tools/ext4/tier1_xfstests_docker.py` now emits this source preparation order:

1. `make configure`, then the existing glibc-compatible `./configure` call.
2. `make -C lib` to materialize `lib/libtest.la`.
3. Grouped `make -j2 -C ltp ...` and `make -j2 -C src ...` calls for only the
   helpers selected by the Tier 1 xfstests cases.

`tools/tests/test_ext4_fault_matrix_runners.py` locks the generation order and
directory-local helper invocation.

## Verification

- `python3 tools/tests/test_ext4_fault_matrix_runners.py` passed 12 tests.
- `cargo xtask ext4 tier1 --preflight-live --preflight-report target/ext4/reconciliation-preflight-after-helper-build.json` passed with zero blockers.
- The preflight report records `xfstests_source_prepared`,
  `xfstests_selected_cases_verified`, `xfstests_linux_execution_ready`, and
  `linux_rw_replay_ready` as true. It is local ignored campaign evidence, not
  a checked-in acceptance receipt.

## Boundary And Next Step

This removes an execution-harness blocker only. It does not satisfy pending
Linux-generated depth-three extent-shape, exchanged Linux/Tx image, rustc
workload, SubmissionManager, or candidate-bound crash evidence. Do not start
the 1000-cut live campaign until those candidate inputs are fixed and the run
uses a timestamped `--run-id` with its `target/ext4/tier1/<run-id>/` evidence
directory preserved for resume.

Global `cargo xtask progress validate` remains blocked by the unrelated missing
`docs/progress/research/2026-08-04-smp-scheduler-readiness-audit.md` reference
from the SMP plan; this work does not add a substitute file.
