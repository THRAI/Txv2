# Ext4 Docker E2fsprogs Candidate Path

Date: 2026-08-09

## Change

The live Tier 1 runner previously accepted Docker Linux replay and xfstests
execution on this host, but still rejected the candidate before execution when
native `e2fsck` and `debugfs` were absent. `tools/ext4/e2fsprogs_docker.py`
now runs either tool in the repository's pinned Linux Docker image with the
image parent mounted read-only. Native e2fsprogs remains the first choice.

`xtask/src/ext4/e2fsprogs.rs` represents either backend as a program plus an
argument prefix. The live runner uses that representation for its immutable
role-image `e2fsck -fn` checks, and the crash executor receives both rendered
commands through its existing environment overrides. The semantic oracle can
therefore use the same read-only Docker `debugfs` path.

## Verification

- `cargo test -p xtask ext4::e2fsprogs::tests::docker_tool_command_preserves_prefix_before_e2fsprogs_arguments -- --exact --test-threads=1`
- `python3 -m unittest tools.tests.test_ext4_fault_matrix_runners`: 14 passed.
- A Docker-created 16 MiB ext4 image passed the wrapper's `e2fsck -fn` and
  `debugfs -R 'stat <2>'` calls.
- `cargo xtask ext4 tier1 --preflight-live --preflight-report target/ext4/reconciliation-preflight-docker-e2fsprogs.json`: passed; the report records Linux replay and xfstests Docker readiness plus CoW storage capacity.

This makes the candidate entry path executable on the current host. It is not
crash-cut evidence, a candidate receipt, or an M1/M3 completion claim.
