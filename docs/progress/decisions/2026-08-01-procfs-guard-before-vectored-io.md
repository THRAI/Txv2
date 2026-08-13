# Procfs Caller Guard Before Vectored I/O

Date: 2026-08-01

## Context

The procfs projected-read boundary already received the step-local caller
`Guard`, but ordinary renderers dropped it. Process-backed renderers then used
VM and process helpers whose no-argument compatibility paths borrowed the
ambient guard or opened a new epoch window. This did not satisfy
`txdoc:INV-V5-LANE`'s caller-owned projection rule.

The current `readv` family separately re-enters scalar syscalls per iovec. A
dedicated vectored operation should eventually compose `IoVecProgress`, shared
open-file position ownership, range submission, and multi-page/SG completion.
Those lower I/O ownership seams are not yet converged.

## Decision

1. Fix procfs first by passing the caller guard explicitly through the renderer
   and every VM/process observation it reaches.
2. Keep no-argument VM/process helpers only as compatibility entry points for
   callers that do not already own a guard; procfs must use the explicit
   `*_with_guard` forms.
3. Defer the six-syscall `readv`/`writev`/`preadv`/`pwritev`/`preadv2`/
   `pwritev2` redesign until the independent `IoSubmissionManager` and correct
   multi-page/SG completion ownership have landed.
4. Do not treat repeated scalar syscall calls as the final vectored-I/O design.
   When resumed, import and validate the iovec plan once and drive a dedicated
   vectored operation instead.

## Verification And Resume Gate

- Procfs guard work: focused process-renderer regression, full `tx-fs` host
  tests, and `cargo -q xtask unit`.
- Vectored-I/O resume gate: one production `IoSubmissionManager` owner, correct
  completion fanout for every page/vector, and a stable range/vector submission
  contract that PageBacked can consume without bypassing its I/O manager.

No blocker remains for the procfs guard correction. Vectored I/O remains
intentionally deferred behind the resume gate above.
