# 2026-05-26: Raw Linux AIO ABI Supersedes fd-shaped AIO Handles

The earlier PR-11 AIO canary intentionally normalized Linux
`aio_context_t` into an fd-shaped Tx handle. That was useful for proving
`OnBehalfOf<P>` worker plumbing, but it is not compatible with unmodified
Linux userspace.

This decision supersedes that ABI choice: the syscall-visible AIO target is
now raw Linux RV64. `io_setup(nr_events, ctxp)` validates and writes a
nonzero `aio_context_t` user handle, and `io_submit`, `io_getevents`,
`io_pgetevents`, `io_cancel`, and `io_destroy` look up that handle directly.
The internal `AioContext` cap remains private implementation state.

The initial v1 ring mapped as Tx private-anon memory while the raw ABI and
completion path were being brought up. On 2026-05-27 the ring moved to an
anonymous PageBacked container mapped `SHARED`; the user-visible layout still
follows Linux: `struct aio_ring` header at the context address, page-rounded
event capacity, completion events published before tail updates, and
`io_getevents` advancing the user-visible head.

Follow-up update later the same day: raw AIO completion now uses the intended
reactor shape. `io_submit` is admission-only; the per-context AIO worker future
drains IOCBs under `OnBehalfOf<P>`, publishes completions into the raw ring,
signals `IOCB_FLAG_RESFD` eventfds, and wakes `io_getevents`. The boot reactor
installs an AIO worker submit hook next to the child-thread submit hook; host
tests without that hook keep the explicit worker-registry pump.

Follow-ups:

- Extend `io_cancel` beyond exact queued-request cancellation into deeper
  cooperative interruption for long-running backend operations.
- Continue `io_uring` registration and ring-depth work; AIO's raw ring now
  exercises the shared PageBacked user-copy path but is not an `io_uring`
  implementation.
