---
name: tx-ltp-timeout-ladder
description: Use when running or debugging txKernel LTP/OSComp QEMU tests, especially focused syscall or network batches, to choose short timeouts, avoid wasting time on silent hangs, preserve serial logs, and escalate only when a run shows progress.
---

# tx-ltp-timeout-ladder

Use this skill before launching LTP/OSComp QEMU runs. It keeps the loop short:
focused case lists first, 30s timeout first, and log every run with a stable
path.

## Read First

- `docs/LTP/ltp-network-syscall-progress.md`
- `docs/LTP/ltp-batches.md`
- `docs/LTP/ltp-network-deferred.md` for socket/network syscall cases
- `tools/oscomp-judge.py`
- Pair with `tx-ltp-syscall` and, for socket cases, `tx-network-test-fixup`.

## Timeout Ladder

- Start with `timeout 30s` for a focused LTP case or small batch.
- If the run emits no useful boot/LTP progress within 30s, treat that as a
  hang clue. Stop, inspect the last serial line, and identify the current case.
- Move to `60s` only after the 30s run showed forward progress.
- Move to `120s` for known slow focused cases or multi-case batches that are
  already printing case output.
- Use `300s` only for aggregate confirmation after focused runs are understood.
- Do not start with long blind timeouts for debugging. Long runs hide the first
  stuck case and waste the iteration budget.

## Command Shape

Always give the serial log a descriptive name:

```sh
timeout 30s make oscomp-local-rv64 \
  OSCOMP_LTP=case01,case02 \
  OSCOMP_OUT_RV=target/oscomp/ltp-<scope>-<reason>.txt

python3 tools/oscomp-judge.py \
  target/oscomp/ltp-<scope>-<reason>.txt \
  target/oscomp/testdata
```

For network syscall work, do not rely on `LTP_BATCH=net`; the ordinary batch
view filters those cases. Use `OSCOMP_LTP=...` or a runner path that is known
not to truncate the case list.

## Hang Triage

1. Find the last `RUN LTP CASE ...` line in the log.
2. Read the LTP source for that case before changing kernel code.
3. Classify the blocker: ABI/user-copy errno, socket state, dataplane,
   readiness/wakeup, fd lifecycle, harness environment, or unsupported
   protocol.
4. If there is a trap/panic, run `cargo xtask fault-decode --target rv64-qemu
   --serial <log>`.
5. Fix the semantic owner. Do not branch on case names, argv, fixed payloads,
   or expected errno sequences.

## Completion

Before reporting a run as useful, record the command, log path, local judge
score, first failing case, and whether the timeout was 30/60/120/300s. If a
longer timeout was needed, say why.
