# LTP network/socket deferred cases

This file records LTP cases that are intentionally ignored during the current
non-network bringup pass. They are not considered solved. Keep them out of fast
debug batches, then revisit them when the socket/network module becomes a
target.

## Policy

- Do not spend current LTP iteration time on socket/network-dependent cases.
- Keep them documented here instead of silently dropping them.
- Manual `OSCOMP_LTP=...` can still run any case for investigation.
- Batch filtering in `tools/ltp-batches.py` skips these during normal batch runs.

## Deferred socket/network prefixes

These prefixes are excluded from normal LTP batches for now:

- `accept`, `accept4`
- `bind`, `connect`, `listen`
- `getpeername`, `getsockname`, `getsockopt`, `setsockopt`
- `recv`, `recvfrom`, `recvmsg`, `recvmmsg`
- `send`, `sendmmsg`, `sendmsg`, `sendto`
- `socket`, `socketcall`, `socketpair`

## Epoll cases with socket dependencies

These are epoll tests, but they depend on socket/socketpair behavior:

- `epoll_pwait01`
- `epoll_pwait02`
- `epoll_pwait03`
- `epoll_pwait04`
- `epoll_wait05`

Observed `epoll_pwait01` failure:

```text
epoll_pwait01.c:62: TBROK: The FILE '/proc/6/stat' ended prematurely
epoll_pwait01.c:38: TFAIL: do_epoll_pwait() returned -1, expected 1
epoll_pwait01.c:72: TBROK: read(10000,0x40628c2f,1) failed, returned 0: ENOSYS (38)
```

Diagnosis:

- The test uses `socketpair(AF_UNIX, SOCK_STREAM)` for parent/child
  synchronization.
- Txv2 currently has only a fake/minimal socket shim, not a real socket fd
  implementation integrated with the common fd/read/write/epoll paths.
- This makes the test slow or broken even though the surrounding epoll pieces
  may be working.

Later work needed:

- Decide whether sockets become real `OpenFile` backings or stay as a separate
  subsystem with a proper fd bridge.
- Implement `socketpair` read/write semantics.
- Make socket readiness visible to `poll`/`ppoll`/`epoll`.
- Make `close` tear down socket fds cleanly.
- Re-run `epoll_pwait01..04` and `epoll_wait05` after socket readiness works.

Observed `epoll_wait05` spillover during p0:

```text
RUN LTP CASE futex_wait01 : futex_wait01
futex_wait01.c:69: TINFO: Testing variant: syscall with old kernel spec
epoll_wait05.c:43: TBROK: tst_checkpoint_wake(0, 1, 10000) failed: ETIMEDOUT (110)
```

Diagnosis:

- `epoll_wait05` contains socket/socketpair paths (`Polling on socket`,
  `Hang socket`) plus LTP checkpoint synchronization.
- When socket readiness is incomplete, its child/checkpoint path can time out
  after the runner has already moved on, making the next case look stuck.
- It is therefore excluded from p0 alongside the other socket-dependent epoll
  cases.

## Other deferred epoll item

- `epoll01`

Reason:

- `epoll01` maps to the legacy binary `epoll-ltp`.
- It is slow in the reference run and currently times out in Txv2 local runs.
- It should be investigated separately after the faster non-network epoll cases
  are stable.

Observed:

```text
RUN LTP CASE epoll01 : epoll-ltp
TIMEOUT LTP CASE epoll01
FAIL LTP CASE epoll01 : 124
```

## Current non-network epoll focus

Keep working on these first:

- `epoll_create*`
- `epoll_create1_*`
- `epoll_ctl*`
- `epoll_wait*`

Known note:

- `epoll_wait02` now mostly works after timeout handling; local detailed scoring
  may still count unrelated `TCONF` lines such as `clock_getres()` in the
  denominator.
