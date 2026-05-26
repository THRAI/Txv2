# Netfilter Namespace Ownership Checkpoint

Date: 2026-05-19

## Context

The N72N2/N72O0-O1 slice made `NETLINK_NETFILTER` capable of creating a small
nf_tables-shaped ruleset, but the underlying txKernel netfilter model still
used process-global rule and conntrack vectors. That was enough for one Docker
bridge, but it was the wrong ownership model for multiple container network
namespaces.

## Current Choice

`NetNamespacePayload` now owns a `NetfilterState`:

- filter/NAT rules
- MASQUERADE conntrack entries
- DNAT conntrack entries
- per-rule packet and byte counters

The old helper API names remain as initial-netns wrappers for existing
bootstrap and focused tests. New namespace-aware helpers are used by namespace
forwarding, namespace cleanup, rtnetlink teardown, and nfnetlink socket send.

The nfnetlink table/chain staging metadata is also keyed by network namespace,
so creating a rule through a NETLINK_NETFILTER socket mutates the socket's
namespace instead of a shared global model.

## What This Proves

- A host namespace can own Docker-style MASQUERADE/DNAT state independently of
  container namespaces.
- A netlink socket created in an isolated namespace sees and mutates that
  namespace's ruleset.
- Rule counters are updated on matching filter and NAT rules and are visible in
  `/proc/net/tx_nf_rules`.

## Remaining Caveat

Procfs network projections still render the initial namespace because the
projected read/write path does not yet carry caller process namespace context.
The netfilter control parser itself accepts a `NetNamespacePayload`, so the next
step is plumbing caller-netns context through procfs rather than inventing a
separate control plane.

## Verification

- `cargo fmt`
- `cargo fmt --check`
- `git diff --check`
- `cargo test -p tx-subsystems nfnetlink`
- `cargo test -p tx-subsystems netfilter`
- `cargo test -p tx-subsystems bridge`
- `cargo test -p tx-shims netlink`
- `cargo build -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
