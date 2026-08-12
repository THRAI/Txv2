# Appendix A — Design-vs-code ledger

This tutorial is faithful to the code, not to `HAL_v1.md` where the two have
drifted. Here is the consolidated list of every place the shipped implementation
has outgrown the design document, so the tutorial doubles as a doc-drift audit.
Each entry names the doc's claim, the shipped reality, the file, and the chapter
that covers it.

Treat this as an actionable patch list for bringing `HAL_v1.md` back in sync. None
of these are bugs in the code; they are the document lagging the implementation.

## Supertrait membership

| Doc says | Code does | Where | Ch |
|---|---|---|---|
| `TxPlatform` aggregates the listed axes (no `EntropyIf`, no `ObserverIf`) | Supertrait also includes `EntropyIf` and `ObserverIf` | `crates/tx-hal/src/lib.rs:1386` | 2, 14 |
| `PowerIf` has `system_off`, `reboot`, `cpu_off` | `PowerIf` has only `system_off()` | `crates/tx-hal/src/lib.rs:1382` | 14 |
| `FpSimdIf` excluded from supertrait | ✅ matches — `FpSimdIf` is reached for separately | `crates/tx-hal/src/lib.rs:1077` | 2 |

## `PlatformConfig`

| Doc says | Code does | Where | Ch |
|---|---|---|---|
| Associated constants with no defaults | Most constants have trait defaults so a minimal platform compiles | `crates/tx-hal/src/lib.rs` `PlatformConfig` | 3 |
| (not mentioned) | Adds `SUBSTRATE_BOOT_READY: bool = false`, gating the substrate-backed boot | `crates/tx-hal/src/lib.rs`; board sets `true` at `lib.rs:272` | 3, 6, 15 |

## `PmapIf` — the largest divergence

| Doc says | Code does | Where | Ch |
|---|---|---|---|
| Presents a "full planned" surface with associated types `type Pte; type PmapRoot; type Asid;` | **Never built.** Uses concrete shared `tx-hal` types: `PmapRoot`, `Asid(u16)`, `PmapReservation`, `PmapInvalidation`, `PmapUnmapResult` | `crates/tx-hal/src/lib.rs:670` | 7 |
| `PmapCommitBatch<P>` aggregator for batched commits | No batch type; commits are single-mapping | — | 7 |
| `PmapReservation` frees intermediates via a `Drop` impl | No `Drop`; cleanup is the explicit `rollback_*` call (`#[must_use]` nudges it) | `crates/tx-hal/src/lib.rs:513` | 7 |
| `commit_*` signature without permissions | `commit_kernel_mapping`/`commit_mapping` take an extra `PmapPermissions` argument | `crates/tx-hal/src/lib.rs:705`, `:758` | 7, 8 |
| `PmapReservation<P>` generic over platform | Not generic | `crates/tx-hal/src/lib.rs:513` | 7 |

## `TrapIf` — structural divergence

| Doc says | Code does | Where | Ch |
|---|---|---|---|
| `TrapIf` has `type RawTrapFrame;` (associated, opaque) + `classify`/`view`/`view_mut` on the trait | No associated frame type; raw frame is the concrete public `Rv64TrapFrame`; trait has `install_*_trap_vector`, `classify_trap`, `snapshot_trap`, `enter_userspace_with_context` | `crates/tx-hal/src/trap.rs:325`; `boards/.../trap.rs` | 9 |
| Generic `rust_trap_entry<P, K>` in the platform crate | `dispatch_trap_frame::<K>` in the platform crate (generic over sink only) + a `#[no_mangle] tx_kernel_riscv64_qemu_trap_dispatch` bridge **in the binary** | `boards/tx-kernel-riscv64-qemu-virt/src/main.rs:22`; `boards/.../trap.rs:848` | 1, 9 |
| `KernelTrapSink::on_timer_interrupt(cpu)` | `on_timer_interrupt(cpu, view)` — gains the trap-frame view for userspace preemption | `crates/tx-hal/src/trap.rs:316` | 9, 10 |
| `TrapFrameMutVtable` write methods (base set) | Adds `capture_user_context`, `restore_user_context`, `set_signal_handler_regs`, `rewind_pc` | `crates/tx-hal/src/trap.rs:207` | 9, 10, 12 |

## User access / fixup

| Doc says | Code does | Where | Ch |
|---|---|---|---|
| §12: `UserAccessIf`, `KernelPtr<T>`, `FixupEntry`, kernel-mode fault recovery all **retired**; eager walk replaces them | Eager walk is the main path (in VM, `AddressSpace::copy_*_user`) ✅; **but** a board-internal 2-entry `RV64_FIXUP_TABLE` survives for synchronous signal-frame writes | `crates/tx-subsystems/src/vm/user_access.rs`; `boards/.../user_access.rs:157` | 11 |
| §21.1 lists `KERNEL_FIXUP_TABLE` as the one allowed trap-consulted **`linkme`** slice | No `linkme` fixup slice; the surviving table is a plain `static [_; 2]` consulted directly in the trap shell. (The §12 note already acknowledges the exception — §21 contradicts §12; code matches §12.) | `boards/.../user_access.rs:176` | 11 |

## SMP

| Doc says | Code does | Where | Ch |
|---|---|---|---|
| `IpiKind` enumerates the cross-hart messages (no `Membarrier`) | Adds `Membarrier` (for `membarrier(2)`) alongside `Reschedule`/`TlbShootdown`/`Stop` | `crates/tx-hal/src/lib.rs:1295` | 14 |

## Boot / mainline

| Doc says | Code does | Where | Ch |
|---|---|---|---|
| `entry` re-reads `boot_info()` with a `debug_assert!` after `boot_handoff` | Omitted; validation moved into the `BootStaticBag` high-sentinel check + DTB fallback | `crates/tx-hal/src/lib.rs:1450` | 6 |
| `kernel_main` is a linear skeleton | One-line delegate to `init::CoreInit::<P>::boot`; the linear chain lives in `init_substrate_if_ready`, gated on `SUBSTRATE_BOOT_READY` | `crates/tx-kernel/src/lib.rs:67`; `init.rs:216` | 2, 6 |
| §11: "user execution is still not enabled" | Userspace is live: `run_bootstrap_exec_for_init` + `run_userspace_reactor_loop` | `crates/tx-kernel/src/init.rs:216` | 6, 10 |

## Things the doc got exactly right (worth noting)

Not everything drifted. These doc claims match the code precisely and are worth
trusting:

- The four-crate diamond and board-binary minimalism (Ch 1) — with the single
  documented trap-symbol exception.
- The `TxPlatform` blanket impl and zero-sized-platform static dispatch (Ch 2).
- The address-value-vs-dereference-authority discipline and its `cargo xtask lint
  arch` enforcement (Ch 3).
- The H0–H1 boot stages, the low/high linker split, and the identity-bridge
  rationale (Ch 4).
- The `BootStaticBag` static-ref discipline and no-heap DTB parsing (Ch 5).
- The `TrapFrameMut` vtable *rationale* (confine indirection to rare writebacks),
  even though the surrounding `TrapIf` shape changed (Ch 9).
- The eager-walk replacement of the general fixup table (Ch 11).
- The HAL/`SMP_v1` mechanism-vs-policy boundary (Ch 14).
</content>
