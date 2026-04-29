# RV64 Identity Teardown Sentinel

**Date:** 2026-04-28

## Decision

RV64 QEMU now removes the temporary low identity root entry before generic
kernel work begins. The ordering is:

1. `_start` builds the bootstrap page table with identity, direct-map, and
   high-kernel aliases.
2. `_start` installs `satp`, rewrites `sp`/`gp`, and jumps to high
   `rust_entry`.
3. `BootPlatformIf::boot_handoff` publishes static `BootInfo` from the
   `BootStaticBag<IdentityLive>` DTB pointer while the OpenSBI DTB is still
   reachable through the low identity bridge.
4. The teardown sentinel reads current `pc`, `sp`, and `gp`; all must be inside
   the high kernel alias window.
5. The sentinel clears the QEMU RAM identity root entry, updates
   `BootstrapPmapInfo.identity` to `None`, executes `sfence.vma`, and stores a
   `BootStaticBag<IdentityDropped>` as the only remaining boot-static
   authority.

`BootLinkedAddr::from_runtime_addr()` canonicalizes runtime static pointers
back to linked/physical address facts during the single construction step. The
bag is not captured again after the high jump; later users borrow the stored
identity-dropped bag, which retains the DTB only as a raw value fact.

## Consequences

- H3/substrate code no longer depends on the low identity bridge.
- The raw `BootHandoff.firmware_arg` remains observable as a firmware value,
  but consumers must use published `BootInfo`, not dereference it as a pointer.
- The next pmap slices can focus on permission splitting, direct-map extension,
  reserve/commit/unmap, and shootdown integration.

## Verification

- `cargo test -p tx-hal-riscv64-qemu-virt`
- `cargo test -p tx-substrate`
- `cargo test -p xtask`
- `cargo fmt --check`
- `cargo check -p tx-hal-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
- `cargo xtask build --target rv64-qemu`
- `cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel --timeout-ms 10000`
- `cargo xtask lint arch`
- `cargo xtask lint docs`
- `cargo xtask progress validate`
- `git diff --check`

## Next Step

Split the coarse high kernel alias into final permissions and make the direct
map extensible from BootInfo memory ranges.
