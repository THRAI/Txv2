# EBR and Zone first executable slice

## Context

The object model and step discipline assume a substrate layer that can publish
typed objects, hold retained identity evidence, observe weak references under
epoch guards, and delay physical reuse until guard-scoped readers have
quiesced. The imported EBR/Zone references describe the target mechanics, but
the kernel needed an executable first slice before process, thread, mount, VFS,
and reactor work could start consuming those APIs.

## Decision

`tx-substrate` now owns the first executable EBR/Zone implementation.

The EBR slice provides `epoch::guard`, a global epoch counter, per-CPU
retired-node slices, bounded current-CPU drain, and raw retirement callbacks
used by Zone. Guards publish the current epoch and pin the CPU while
guard-scoped observations are alive.

The Zone slice provides static `Zone<T>` instances, `ZoneRegistry`, frame-backed
`ZoneSlab<T>` pages, bitmap slot allocation, `ZoneReservation<T>` rollback,
infallible `sign`, compact retained handles, weak observations, and EBR-delayed
slot/slab reclamation. `Cap<T>` stores a 32-bit `zone_id + slot_id` raw key.
`Weak<T>` stores the same raw key plus a generation snapshot. Compile-time
assertions keep `Cap<()>` at 4 bytes and `Weak<()>` at 8 bytes.

The compact key layout is:

```text
raw[31:24] = zone_id - 1
raw[23:0]  = slot_id
slot_id    = (slab_id - 1) * 64 + slot_index
```

The first kernel-side smoke path registers a test zone, reserves and signs one
object, downgrades it to `Weak`, observes it under an epoch guard, upgrades the
`IdentRef` back to `Cap`, drops the retained handles, drains EBR, and prints
`txkernel:zone:smoke:ok` on RV64 QEMU before the normal boot sentinel.

## Consequences

- Upper subsystem work can start depending on the basic role-shaped surface:
  `Zone<T>`, `Cap<T>`, `Weak<T>`, `IdentRef<'g, T>`,
  `ZoneReservation<T>`, `reserve`, and `sign`.
- Static zones are still registered explicitly or lazily. The final
  linker-section collection of every static `Zone<T>` is not implemented yet.
- The implementation is a first executable slice, not the final concurrency
  proof. SMP stress, timer-driven drain integration, and upper-subsystem
  object manifests remain future work.
- The broader `zone-epoch-bus-foundation` plan is only partially satisfied:
  epoch and zone now exist, while bus, index, and mutation substrate pieces
  remain pending.

## Verification

- `cargo fmt -p tx-substrate -p tx-kernel --check`
- `cargo check -p tx-kernel --offline`
- `cargo check -p tx-substrate --offline`
- `git -C Txv2 diff --check`
- `cargo xtask build --target rv64-qemu`
- `cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel`
- `grep -a 'txkernel' target/qemu-rv64-qemu-smoke.serial.log`

The RV64 serial log includes both:

```text
txkernel:zone:smoke:ok
txkernel:qemu-riscv64-virt:boot:ok
```

## Next

Replace the placeholder kernel smoke with subsystem-owned zone registrations as
Process, Thread, Mount, VFS, and VM objects land. Add the linker-section or
macro-based static zone registration mechanism, then continue the substrate
foundation plan with index, mutation, bus, and reactor-facing wake carriers.
