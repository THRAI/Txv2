# Portable board / main merge semantic audit (2026-08-18)

## Scope

This audit covers the uncommitted merge on
`integration/portable-net-vf2-dwmac-main`. The main branch remains authoritative
for VM, filesystem, syscall, scheduler, reactor, and page-cache design. The board
branch contributes static device resources, transactional binding, physical
drivers, typed interrupt routes, DMA/cache semantics, and board boot support.

## Corrected integration defects

- User-copy and VM range waiting now retries the semantic operation when the
  completion source retires between observing `Yield` and resolving its
  registry carrier. That race no longer becomes an unrelated userspace `EIO`.
- The RV vDSO coarse-clock success path jumps to its normal return instead of
  falling through to the `ENOSYS` fallback.
- The vDSO build accepts the installed GNU RISC-V assembler when the musl-named
  assembler is absent. The assembly is freestanding, so this changes tool
  discovery rather than its ABI. The release kernel now embeds the 4 KiB vDSO
  instead of a stub.
- Existing merge repairs retain main's async PageBacked/File-I/O semantics while
  preserving owner retirement revalidation, task-generation checks, transactional
  user-range reservation, futex wake-source identity, and rollback behavior.

## Device and interrupt audit

The selected platform still forms a static, build-time HAL bundle. Resource
descriptions are validated before binding; binding reserves resources
transactionally; devices are published before activation; and controller lines
are unmasked last. Block, network, and character devices continue to enter the
existing registries rather than adding board branches above the device layer.

Typed device IRQ routes are dispatched before the legacy platform fallback, so
UART/RTC and not-yet-migrated sources remain reachable. Deferred network IRQ
handling masks the line, records the claim, wakes the bottom half, acknowledges
the device, completes the controller claim, and then unmasks the line.

One deliberate limitation remains: the route table supports shared IRQs, but a
hart currently has one deferred-network claim slot. Two network handlers that
both defer on the same asserted shared line would collide. Current QEMU, VF2,
and 2K1000 configurations expose one physical NIC on its device IRQ, so the
condition is outside the supported resource graphs. A future multi-NIC shared
line must use a bounded claim queue or be rejected during graph validation.

The EBR implementation was also reviewed across nested guards, sparse CPU masks,
generation-tagged retired slots, scan ordering, and IRQ/trap boundaries. No
additional lock or fallback was added; no evidence identified it as the cause of
the reproduced failures.

## Verification

- `cargo -q xtask unit`: tx-kernel 201/201, tx-ext4 102/102 with 2 ignored, and
  tx-scripts 171/171; the remaining 37 tx-shims failures match clean main.
- Focused retired-wait-source and vDSO branch regressions: 1/1 each.
- tx-hal 12/12; tx-drivers 49/49; RV QEMU HAL 121/121; LA QEMU HAL 75/75;
  LA2K1000 HAL 31/31.
- Optimized builds passed for RV QEMU, LA QEMU, and LA2K1000 kernel-only.
- Full RV final-stage run used QEMU 9.2.1, 4 GiB, 8 harts, and the existing image
  in direct-write mode. CAgent passed 10/10. BuildStorm completed in 998.40 s,
  produced a 1,681,000-byte artifact, and final init reported
  `cagent_rc=0 buildstorm_rc=0`.

Serial log:
`target/oscomp/rv-full-portable-merge-audit-vdso-r5-20260818.log`.

The merge remains uncommitted. Platform judge and physical-board validation must
refer to the eventual merge commit rather than this working-tree evidence.
