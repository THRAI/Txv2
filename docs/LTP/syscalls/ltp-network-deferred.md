# LTP network/socket deferred cases

This file records LTP socket/network cases that are excluded from the normal
non-network syscall batches. They are not considered solved. When the
socket/network module is the current target, run them explicitly with
`OSCOMP_LTP=...` instead of relying on `LTP_BATCH=net`.

## Policy

- Keep socket/network cases out of the ordinary non-network batches so they do
  not destabilize VFS/VM/process bring-up.
- Do not interpret `make ltp-batches` showing `net 0` as "no network tests";
  it means the 50 known network/socket syscall entries are filtered from the
  active batch view.
- Run focused network work with `OSCOMP_LTP=...`, for example
  `OSCOMP_LTP=socket01,socket02,listen01`.
- Batch filtering in `tools/ltp-batches.py` skips these during normal batch runs.

## Syscalls Inventory

The current OSComp LTP `runtest/syscalls` list contains 50 socket/network
entries before filtering:

- `accept`: `accept01`, `accept02`, `accept03`, `accept4_01`
- `bind`: `bind01`, `bind02`, `bind03`, `bind04`, `bind05`, `bind06`
- `connect`: `connect01`, `connect02`
- `getpeername`: `getpeername01`
- `getsockname`: `getsockname01`
- `getsockopt`: `getsockopt01`, `getsockopt02`
- `listen`: `listen01`
- `recv`: `recv01`
- `recvfrom`: `recvfrom01`
- `recvmmsg`: `recvmmsg01`
- `recvmsg`: `recvmsg01`, `recvmsg02`, `recvmsg03`
- `send`: `send01`, `send02`
- `sendmmsg`: `sendmmsg01`, `sendmmsg02`
- `sendmsg`: `sendmsg01`, `sendmsg02`, `sendmsg03`
- `sendto`: `sendto01`, `sendto02`, `sendto03`
- `setsockopt`: `setsockopt01` through `setsockopt10`
- `socket`: `socket01`, `socket02`
- `socketcall`: `socketcall01`, `socketcall02`, `socketcall03`
- `socketpair`: `socketpair01`, `socketpair02`

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
