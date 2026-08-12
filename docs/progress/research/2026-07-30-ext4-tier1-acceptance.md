# ext4 Tier 1 Acceptance

Date: 2026-08-05

## Verdict

Task 16 is complete for fresh run `task16-live-retry-20260804`.

The immutable receipt is:

```text
target/ext4/tier1/task16-live-retry-20260804/acceptance-receipt.json
```

Receipt SHA-256:

```text
db1ec817d46704eb969782a0621c2cabd9f07c80e6c3354aed735d37462d72a3
```

Candidate commit at promotion audit:

```text
02294c5d928ada3f504c41d2c0afbe4d55f3faa8
```

## Evidence

- G0-G7 are passed in the verified receipt.
- Crash campaign completed all required deterministic cuts: 1000/1000 across D0-D12.
- TEST, SCRATCH, and WORKLOAD role images passed offline `e2fsck -fn`.
- Per-cut immutable replay image evidence is present and clean for all 1000 crash cuts.
- Docker-backed pinned xfstests passed all 8 selected cases with no failed, skipped, or not-run cases.
- Historical D5 `crash-cut-0213` and D10 `crash-cut-0725` blockers did not recur.

## Verification

Receipt-only verifier:

```sh
/usr/bin/time -p target/release/xtask ext4 tier1 --verify-receipt target/ext4/tier1/task16-live-retry-20260804/acceptance-receipt.json
```

Result: passed (`real 598.54` in the first full receipt-only verifier run; a later receipt-only verifier rerun also exited successfully).

Additional completed gates:

```sh
python3 -m unittest tools.tests.test_ext4_fault_matrix_runners tools.tests.test_ext4_fault_qemu_executor
cargo test -p xtask tier1_verify_receipt_accepts -- --test-threads=1
cargo test -p xtask ext4 -- --test-threads=1
cargo -q xtask unit
target/release/xtask progress validate
target/release/xtask lint docs
```

## Follow-Up Rule

Do not rerun a completed multi-hour crash prefix while the run-owned evidence is still intact. For immutable evidence, prefer:

```sh
target/release/xtask ext4 tier1 --verify-receipt target/ext4/tier1/task16-live-retry-20260804/acceptance-receipt.json
```

For a future live failure, resume at the first failed cut:

```sh
cargo xtask ext4 tier1 --run-id <run-id> --resume --start-cut crash-cut-NNNN
```

Only rerun the full campaign if the retained prefix evidence, receipt lock, artifact manifest, or authority inputs are invalidated.

## Next

Tier 1 is now the closed fast product gate for the bounded 4 KiB metadata_csum, ordered-JBD2, depth-one extent, non-splitting directory, and classic-orphan profile. Tier 2 should start from explicit out-of-scope expansion: deeper extent/htree growth, `orphan_file`, xattr/ACL/quota/fallocate/direct I/O/DAX/fast-commit, multiple concurrent transactions, and broader performance promotion.
