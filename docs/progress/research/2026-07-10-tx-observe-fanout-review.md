# tx-observe fanout review

Date: 2026-07-10

Scope: read-only fanout review of the tx-observe kernel producer API/ABI,
host drain/replay/export pipeline, analyzer/cache surface, active Txv3 docs,
and progress-memory consistency.

## Summary

The core producer path is still structurally sound for the main hot-path
contract: O(1), stack-only, non-blocking, Release-published records, explicit
padding, and compile-time layout assertions for the fixed 80-byte record ABI.
The main improvement areas are not basic ring mechanics; they are loss
semantics, typed interface boundaries, host/export completeness reporting, and
active-doc drift after raw-first live drain landed.

## Findings

1. `span_begin()` returns a usable `SpanId` even if the begin record was dropped
   due to a full ring. `HartEmitter::span_begin` mints the span before calling
   `emit`, while `emit` returns no status after incrementing `lost`. Follow-on
   children and `span_end` calls can therefore produce orphaned records that look
   like real spans. Next fix: make the producer publish path return a status and
   make span begin return `Option<SpanId>` or `SpanId::NONE` on drop.

2. `span_end()` hardcodes `TxTraceLevel::Boundary` and `name = 0` for every
   close record, including drive, step, yield, phase, and sched spans. This
   weakens filtering and host reconstruction. Next fix: carry close metadata in
   the span handle or accept explicit typed close metadata.

3. The producer API still accepts arbitrary `(TxPayloadTag, &[u8])`, and `emit`
   silently truncates payload bytes to the 16-byte inline budget. This is too
   loose for a frozen ABI. Next fix: route public emitters through typed payload
   constructors that bind tag, expected length, and payload bytes with debug
   assertions.

4. The docs describe a per-hart reentry guard, but the producer implementation
   does not currently enforce it. Current emit helpers appear bounded, but the
   invariant is not mechanically protected against future helper regressions.

5. txtrace-v0 has grown new level/tag surface such as sched and process payloads
   while the canonical serialization document still partially reflects older
   tables. Either document these as valid v0 extensions everywhere or bump the
   relevant version axis.

6. Live raw-only captures can report misleading completeness. In raw-only mode,
   `drained.raw_records`, `drained.lost_records`, and
   `drained.overwritten_records` carry the hot-path evidence, but the CLI return
   path and summary emphasize post-stop ring `stats.complete`. Runtime metadata
   needs a top-level combined `complete/lossless` summary over drained and final
   stats.

7. Perfetto export drops `Counter` records even though the kernel emits
   `CounterValue` and the analyzer consumes them. NDJSON/analyzer and `.pftrace`
   therefore disagree on diagnostic completeness.

8. Replay sorting moves repair records to the end because repair events sort by
   `u64::MAX`. Damage markers should stay adjacent to the damaged slot or carry
   a best-effort timestamp/sequence key.

9. `xtask observe replay --out pftrace` and `xtask observe pftrace` are not
   equivalent: the daemon supports names on replay, but the replay wrapper does
   not forward names/fallbacks while the pftrace wrapper does. The two user
   surfaces should share one implementation.

10. Active docs still describe `live-guest-mem` as always post-processing
    NDJSON/PFTrace, while the current code and intended workflow are raw-first
    with `--finalize` opt-in. The active wrapper table also omits `observe
    analyze`, leaving SQL/Python/Parquet/cache surfaces underdocumented.

11. `--python-file --parquet-dir` can write Parquet tables without the manifest
    that the normal cache path uses, so those tables are durable but not
    reusable through the manifest fast path.

## Suggested order

1. Fix producer loss semantics and span close metadata first, because they
   affect every downstream view.
2. Tighten payload construction and add the missing reentry guard.
3. Fix raw-only runtime completeness and Perfetto counter export.
4. Unify `xtask observe replay --out pftrace` with `observe pftrace`.
5. Update active docs and progress blocker wording after code behavior is
   aligned.

## Verification

This was a read-only review. One analyzer-side reader ran:

- `python3 -m py_compile tools/tx-observe-analyze.py tools/tests/test_tx_observe_analyze.py`
- `python3 -m unittest tools.tests.test_tx_observe_analyze`

Both passed. The main thread only inspected source, docs, tests, and progress
records, then wrote this research note and a status pointer.

## Blockers

No implementation blocker was hit. The checkout is very dirty and includes
unrelated observe, timer, network, HAL, filesystem, and syscall work, so any fix
should be scoped by file and verified with focused commands before staging.
