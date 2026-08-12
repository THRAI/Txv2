# Debian In-Guest StarryOS Readiness Gap

**Date:** 2026-07-15

**Scope.** Assess the path from txKernel RV64 QEMU boot to a minimal Debian
guest that installs a full Rust toolchain, builds StarryOS, then launches
StarryOS in an inner RV64 QEMU instance. This is a read-only readiness audit;
it changes neither kernel behavior nor image policy.

## Decision

**Ready: no.** The current guest proof is Alpine/musl plus a bounded TCC
compile/link/run witness. It is not a normal dynamically linked Debian/glibc
environment, and the first required Debian-class dynamic launch still fails
before vDSO resolution. Consequently neither an in-guest Rust toolchain nor
the nested-QEMU StarryOS workflow has a credible end-to-end witness.

StarryOS is not an RV64 Debian userspace program. Debian must build it with
its bare-metal Rust target and launch an inner `qemu-system-riscv64`; StarryOS
then runs as that inner VM's kernel. The inner QEMU TCG path makes this a much
stronger Linux ABI, filesystem, process/thread, memory, and device workload
than the current Alpine shell tests.

## Evidence Ledger

| Layer | Current evidence | Readiness for the target |
|---|---|---|
| Image and root mount | `smoke`, `busybox`, and `alpine` are the only xtask profiles. Root starts from an initramfs/tmpfs; an attached ext4 `vda` is mounted at `/musl`, not `/`. | Missing Debian root-disk/image-policy path. |
| Persistent writable storage | ext4 direct RW operations exist, but buffered writeback is deferred and the documented durability guarantee is graceful-unmount only. | Insufficient for package manager/Cargo workload proof. |
| Dynamic executables | The exec layer maps `PT_INTERP` and records TLS/dynamic metadata, but leaves dynamic tags, dependency loading, and relocations to userspace. The dynamic-glibc witness fails before resolver/vDSO use. | Blocking. Establish dynamic musl and glibc launch first. |
| Compiler workload | Alpine can list `gcc` and `cc1`; `gcc --version` and an explicit musl-loader invocation time out. TCC has only a limited explicit-CRT witness. | Blocking. Rust is materially beyond this witness. |
| Linux ABI | Mechanical status on 2026-07-15: 243 local syscall numbers, 243 dispatched, 82 absent versus Linux RV64 v6.17. Current thread, signal, VM, procfs, TTY, and network coverage is useful but incomplete. | Broad compatibility campaign required; dispatch count is not ABI completion. |
| Nested StarryOS run | StarryOS requires nightly Rust plus host build tools, its bare-metal RV64 target, rootfs preparation, and a second `qemu-system-riscv64`. | Untested and blocked by every preceding layer. |

## Machine Configurations

| Target | What is known now | Status |
|---|---|---|
| 1 core, 2 GiB | The existing Alpine profile dry-run is 1 core and 1024 MiB. `--smp 1` is accepted, but xtask has no memory override, so 2 GiB needs a small harness/CLI extension or a manual QEMU command. The boot allocator derives its plan from platform memory regions and extends the direct map when required; this is an architectural path, not a 2 GiB boot witness. | Not verified. |
| 8 cores, 8 GiB | `--smp 8` is accepted by the QEMU command builder, but remains paired with the fixed 1024 MiB Alpine profile. Existing project evidence covers SMP slices, not this exact 8-core/8-GiB Debian or nested-QEMU load. | Not verified; no current launch configuration or performance claim. |

StarryOS upstream defaults to RV64 QEMU `virt`, 1 GiB, virtio block/network,
and software QEMU acceleration. Its Makefiles accept `MEM=2G SMP=1` and
`MEM=8G SMP=8`, with the latter enabling its SMP feature, but upstream does
not publish a minimum-resource claim or a witness for either requested
configuration. Treat both sizes as validation targets, not resource budgets.

## Implementation Entry Order

1. Add a reproducible Debian-class root-disk boot harness: choose initramfs
   handoff versus `vda` as `/`, set up persistent writable ext4, and expose
   explicit `--memory` and `--smp` test parameters. First gate: boot to a
   noninteractive Debian shell on 1 core/2 GiB.
2. Close the dynamic executable chain with separate static, PIE, dynamic musl,
   and dynamic glibc witnesses. The immediate blocker is
   `entry-dynamic.exe` before vDSO/resolver use.
3. Establish a compiler ladder: `gcc --version`, compile/link/run a C program,
   `cargo --version`, build/run a small Rust crate, then install/use the
   pinned StarryOS Rust toolchain. Run the corresponding LTP/OSComp cases for
   every syscall/subsystem fix rather than treating shell success as closure.
4. Prove Debian filesystem/process behavior under sustained package/Cargo I/O:
   `fork`/`exec`, pthread/futex/TLS, `mmap`, pipes/poll/epoll, signals,
   procfs/TTY, network download, and graceful remount/reboot persistence.
5. Only then install StarryOS's documented build prerequisites, build its
   `riscv64gc-unknown-none-elf` kernel, and boot an inner QEMU with
   `MEM=2G SMP=1`. Repeat with `MEM=8G SMP=8`, first requiring its serial
   prompt and then running a bounded workload. Record wall-clock, peak guest
   memory, disk consumption, and failures separately; TCG nested performance
   cannot be inferred from configuration alone.

## Sources

- `xtask/src/target.rs` and `xtask/src/qemu.rs` for supported profiles,
  topology, fixed memory defaults, and `--smp` behavior.
- `crates/tx-kernel/src/init.rs` and `crates/tx-ext4/src/mount.rs` for root
  mount, `/musl`, writable ext4, and writeback/durability boundaries.
- `docs/progress/research/2026-06-27-alpine-gcc-readiness.md` for the bounded
  Alpine GCC failure; `docs/progress/research/2026-07-15-vdso-time-abi-crosswalk.md`
  for the dynamic-glibc/vDSO blocker.
- `docs/progress/plans/2026-07-14-elf-exec-loader.json` and
  `crates/tx-scripts/src/process/exec/loader.rs` for current ELF scope.
- `cargo xtask syscall-status` (2026-07-15) for mechanical syscall coverage.
- StarryOS upstream at commit `2e075accf4fb0aefdd1d252ebd9ccf29727d9923`:
  `README.md`, `rust-toolchain.toml`, `Makefile`, and `make/{config,qemu,deps}.mk`.

## Verification and Next Step

This audit ran `cargo xtask syscall-status` and QEMU dry-runs for Alpine with
`--smp 1` and `--smp 8`; both dry-runs retain the fixed `-m 1024M` default.
No guest was booted and no implementation was changed. Validate the research
Markdown with the docs lint; validate progress records without rewriting the
currently dirty operational JSON. The next engineering action is the first
Debian root-disk/dynamic-loader milestone, not StarryOS integration.
