# RV64 Boot Static Bag

**Date:** 2026-04-28

## Decision

RV64 QEMU boot/static address capture is centralized in the board-private
`BootStaticBag`. Rust code outside `boot_static.rs` no longer names linker
symbols, converts `UnsafeCell` static pointers into address facts, or calls
`addr_of!` for bootstrap statics.

`BootStaticBag` is now the board's single source of truth rather than a
repeatable capture helper. In the current high-VMA/low-LMA RV64 path, low
assembly uses only suffixed `_load` linker symbols before translation is
enabled; high Rust then constructs `BootStaticBag<IdentityLive>` exactly once,
passing the OpenSBI DTB pointer into the bag together with the boot stack,
`gp`, `rust_entry`, BootInfo storage, bootstrap root, kernel-alias L1, and
PT-node pool facts. The type is intentionally neither `Copy` nor `Clone`; the
post-entry pipeline consumes the live bag and stores the post-entry authority
for steady-state `BootInfoIf`, `PlatformInfoIf`, and `PmapIf` access. The
transferred bag retains the DTB only as a raw value/provenance fact; the
parser-facing `firmware_dtb()` accessor exists only on the identity-live state.

`BootLinkedAddr` represents an identity-linked boot address before the
high-half jump. It exposes explicit conversions to physical, identity,
direct-map, and checked kernel-alias address values. Bootstrap pmap and
BootInfo publication consume bag methods such as `bootstrap_root_phys()`,
`pt_node_pool_phys_range()`, `boot_info_mut()`, and `kernel_image_phys()`.
Pmap code now receives the live bag during high boot preparation and uses the
transferred global bag only after firmware pointer parsing is gone. PT-node
zeroing uses a bag-derived direct-map pointer on RV64 so post-entry allocation
does not dereference low physical addresses.

Follow-up cleanup keeps the code shape aligned with that authority model. The
low side is now an assembly-only trampoline that builds the identity bridge,
direct map, high kernel alias, and `satp` value from `_load` symbols. The high
side is now a value-consuming post-entry pipeline:
`take_global() -> publish_boot_info_before_identity_drop(firmware_arg) ->
complete_post_entry_pipeline() -> install_global()`. That publishes the
post-entry bag as the steady-state authority without an intermediate mutable
global borrow, and the live path clears the low identity bridge after DTB
publication and the high sentinel.

The portable extraction is the phase contract, not the concrete RV64 storage
layout: every board that needs a firmware/low-to-high transition should have a
single boot-static authority with pre-entry and post-entry pipelines, while the
specific bag fields remain board-private until a second implementation proves a
shared Rust trait is worth freezing.

## Enforcement

The previous grep audit is now a real arch lint. `cargo xtask lint arch`
rejects RV64 QEMU board code outside `boot_static.rs` when it sees `addr_of!`,
`addr_of_mut!`, `.get() as usize`, or Rust `unsafe extern "C"` linker-symbol
blocks.

## Verification

- `cargo test -p tx-hal-riscv64-qemu-virt`
- `cargo check -p tx-hal-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
- `cargo xtask build --target rv64-qemu`
- `cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel --timeout-ms 10000`
- `cargo test -p xtask arch_lint_rejects_rv64_qemu_boot_static_address_leaks`
- `cargo test -p xtask`
- `cargo xtask lint arch`
- `cargo fmt --check`
- `cargo xtask lint docs`
- `cargo xtask progress validate`
- `git diff --check`

## Next Step

Split the coarse high kernel alias into final text/rodata/data permissions and
teach the direct map to extend from normalized BootInfo memory ranges.
