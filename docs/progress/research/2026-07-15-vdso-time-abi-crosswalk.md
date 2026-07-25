# vDSO Time ABI Crosswalk

Date: 2026-07-15
Status: implementation and documentation crosswalk.

## What Changed

Expanded the active contract
[`VDSO_TIME_ABI_v1.md`](../../design/03_memory-vm/VDSO_TIME_ABI_v1.md) to make
the physical mapping transaction, user/kernel address distinction, exec to
auxv hand-off, `tx-vdso` versus `tx-time::vdso` boundary, syscall fallback,
signal-restorer route, file-level responsibility split, and libc-owned
fallback matrix explicit. Current source inspection also confirms that the
boot hook precedes vDSO initialization; the final init publication writes the
first eligible snapshot after the permanent VVAR frame is available.

## Current Evidence

| Claim | Current implementation anchor |
|---|---|
| One VVAR plus immutable image frames map as a single R/RX VM transaction. | `crates/tx-subsystems/src/vm/vdso.rs` |
| `AT_SYSINFO_EHDR` is emitted only from a complete `VdsoMapping`. | `crates/tx-scripts/src/process/exec/script.rs` |
| RV64 fast reads use VVAR plus `rdtime`; unsupported IDs return `-ENOSYS`. | `crates/tx-vdso/src/vdso.S` |
| Slow reads remain syscall 113 through `tx-shims` and the timekeeper. | `crates/tx-shims/src/linux_syscall/time.rs` |
| A live special mapping supplies `__vdso_rt_sigreturn` to signal frames. | `crates/tx-subsystems/src/vm/vdso.rs`, `crates/tx-kernel/src/thread_future.rs` |
| The guest signal witness proves handler RA equals the mapped restorer and scopes ecall 139 to it. | `tools/shell-tests/vdso-phase5-probe.c`, `xtask/src/test.rs` |
| Hook installation precedes `mount_rootfs_from_boot_media()` and `vdso::init()`; its final publication reaches the initialized VVAR frame. | `crates/tx-kernel/src/init.rs`, `crates/tx-kernel/src/vdso/mod.rs`, `crates/tx-subsystems/src/time_hooks.rs` |
| The four-hart stress reader calls the resolved vDSO address directly, while a CPU1 writer updates realtime; its marker binds the disjoint masks, 1024 writes, 500000 reads, direct-vDSO path, completion, and zero reader errors. | `tools/shell-tests/vdso-vvar-smp-probe.c`, `xtask/src/test.rs` |

## Verification

- `git diff --check --no-index /dev/null docs/design/03_memory-vm/VDSO_TIME_ABI_v1.md`
- `cargo xtask lint docs` passed; it reported 7 existing stale-vocabulary
  warnings, which remain non-fatal.
- `cargo xtask progress validate` remains blocked by the unrelated
  `docs/progress/plans/2026-07-14-elf-exec-loader.json` status spelling
  `in_progress` (the schema requires `in-progress`).
- Current revalidation passed `cargo xtask test vdso-witness --target rv64-qemu
  --timeout-ms 60000`, `cargo xtask test vdso-vvar-smp-witness --target
  rv64-qemu --timeout-ms 60000`, and the dynamic-glibc evidence lane. The
  SMP witness uses `CLONE_THREAD` plus bounded shared-memory coordination, so
  it proves VVAR publication rather than depending on unrelated process-exit
  or futex/mailbox teardown behavior. Its boot prerequisite also keeps the
  owner-wake smoke deadline ahead of live AP timer driving until the BSP
  explicitly advances it.

## Closure And Remaining Release Repetition

The four-hart guest-SMP witness supplies the publication proof without shell
child-wait coordination: CPU1 performs 1024 realtime updates while CPU0 makes
500000 calls through the resolved direct-vDSO function, with zero reader
errors. The one-hart fast-path witness separately proves that supported clocks
avoid syscall 113. The dynamic glibc case is now also closed: the
exec interpreter lookup first honors `PT_INTERP`, then recognizes the actual
`/glibc/lib/ld-linux-riscv64-lp64d.so.1` loader path; the guest witness reaches
the dynamic `clock_gettime` completion marker and records no realtime or
monotonic syscall 113 in its case window. The active migration plan is
complete. Future work is release repetition for newly supported boards,
counters, libc/toolchain pairs, and the existing LA64 syscall-only fallback.
