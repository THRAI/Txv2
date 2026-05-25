# Procfs Caller Network Namespace Projection

Date: 2026-05-19

## Context

The N72 netfilter stages moved rule storage, conntrack, and counters into
`NetNamespacePayload`, but procfs still rendered `/proc/net/*` and wrote
`/proc/sys/net/ipv4/ip_forward` through the initial namespace. That was enough
for early host-only tests, but it is wrong for Docker-shaped control flow:
after `unshare(CLONE_NEWNET)` or `setns(fd, CLONE_NEWNET)`, userland expects
network status and sysctl writes to refer to the process's current network
namespace.

## Canonical Gate

| Item | Source / basis | Scope decision |
| --- | --- | --- |
| `FsOps::step_read_projected_with_netns` / `step_write_projected_with_netns` | Existing `FsOps::step_read_projected` / `step_write_projected` backend seam, plus the current process-owned `NetNamespacePayload` model | Add default methods that delegate to the legacy projected path so non-procfs backends keep their behavior. |
| `OpenFileReadOp::caller_netns` / `OpenFileWriteOp::caller_netns` | Existing syscall read/write path already has `SyscallCtx::process`; `Process` owns optional network namespace payload | Carry optional namespace context through the step op, avoiding a global lookup inside VFS. |
| `procfs::read::render_with_netns` | Existing `/proc/net/route`, `/proc/net/arp`, `/proc/net/dev`, `/proc/net/tx_nf_rules`, `/proc/net/nf_conntrack`, and `ip_forward` renderers | Select the supplied caller namespace, falling back to initial netns for legacy direct backend callers and tests. |
| Namespace-aware netfilter procfs render helpers | N72O2/P moved netfilter state into `NetNamespacePayload` | Reuse `netfilter_*_for_namespace` snapshots instead of resurrecting a global firewall table. |

## Current Result

Normal `read(2)` / `write(2)` syscalls now pass `ctx.process.net_namespace()`
into `OpenFileReadOp` and `OpenFileWriteOp`. Procfs consumes that context for
network-sensitive projected files:

- `/proc/net/route`
- `/proc/net/arp`
- `/proc/net/dev`
- `/proc/net/tx_nf_rules`
- `/proc/net/nf_conntrack`
- `/proc/sys/net/ipv4/ip_forward`

Direct backend callers that do not provide caller context still render or mutate
the initial namespace. AIO keeps the legacy path for now because the current AIO
helper does not carry process namespace context through its local read/write
runner.

## Verification

- `cargo fmt`
- `cargo fmt --check`
- `cargo test -p tx-fs procfs`
- `cargo test -p tx-subsystems vfs::execution`
- `cargo test -p tx-shims netns`
- `cargo build -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`

## Next

The next useful stage is a real userland check after `setns`/`unshare`: run an
iproute2/BusyBox shell path that reads `/proc/net/route`, `/proc/net/dev`,
`/proc/net/tx_nf_rules`, and `/proc/sys/net/ipv4/ip_forward` from two different
network namespaces and proves they no longer bleed into each other. After that,
continue toward Alpine image coverage with real `nft`/`iptables` binaries.
