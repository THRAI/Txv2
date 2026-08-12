# vDSO ELF Migration Follow-up

Date: 2026-07-18
Status: implemented narrow correctness fix; larger follow-ups are planned work.

## Scope

This review revisits the RV64 vDSO after the `elf 0.8` exec-loader migration.
It checks whether the migration changes runtime vDSO loading, exposes a new
ELF contract reuse path, or reveals independently actionable ABI defects.

## Result

The runtime ownership split remains correct. Exec parses only untrusted program
images; VM maps the immutable generated vDSO and VVAR transactionally before
the exec point of no return; the stack publishes `AT_SYSINFO_EHDR` only after a
successful `VdsoMapping`. The runtime exec path must not parse or relocate the
vDSO again.

One ABI defect was found and fixed: `CLOCK_REALTIME_COARSE` and
`CLOCK_MONOTONIC_COARSE` seqlock-sampled VVAR bases without checking
`VVAR_CLOCK_MODE_OFFSET`. A counter-ineligible VVAR therefore fell back for
high-resolution clocks but could still return coarse values. The coarse path
now requires `RiscvTime` before reading either base and otherwise returns
`-ENOSYS`, preserving libc-owned syscall fallback for every vDSO clock class.

The VM/exec placement split was also closed. `VdsoLayout` now owns the checked
top-of-user reservation, exec uses it while laying out images and stack, and
the mapper consumes the same value. A low-top platform can no longer reserve
one interval and ask the mapper to search another.

## Evidence

| Claim | Evidence |
|---|---|
| Main/interpreter, stack, and vDSO layout are selected together before mapping; vDSO is mapped before stack construction and before exec publication. | `crates/tx-scripts/src/process/exec/script.rs:308-384`, `971-1019`, `1219-1225`, `1355-1392` |
| vDSO mapping is independently transactional and rolls the whole special range back on recipe/PTE failure. | `crates/tx-subsystems/src/vm/vdso.rs:76-112` |
| The generated image already has the resolver-critical `PT_DYNAMIC`, SysV `DT_HASH`, dynamic symbols and `LINUX_4.15` version definitions. GNU hash is not a current ABI requirement. | `crates/tx-vdso/build.rs:329-442`; `crates/tx-vdso/src/tests.rs:173-246` |
| The exec `Elf08Parser` only supplies Tx-owned ELF header/program-header values; it cannot replace a dynamic-table/version/hash validator. | `crates/tx-scripts/src/process/exec/loader/parser.rs:5-13`; `crates/tx-scripts/src/process/exec/loader/elf08.rs:13-98` |
| `tx-scripts` cannot be imported by `tx-vdso` without an inverse dependency through subsystems. | `crates/tx-scripts/Cargo.toml:13-29`; `crates/tx-subsystems/Cargo.toml:21-31`; `crates/tx-vdso/Cargo.toml:17-21` |

## Follow-ups

1. Add an exec end-to-end host witness for dynamic main plus interpreter:
   `AT_SYSINFO_EHDR == VdsoMapping.vdso_base`, and all main/interpreter/stack/
   VVAR/vDSO intervals are disjoint.
2. Extend the image validator rather than reusing `ExecImagePlan`: check SysV
   bucket/chain reachability and every dynamic address/string range against
   `PT_LOAD`. If shared parser plumbing becomes useful, lower only a
   syntax-level ELF model to a dependency-free crate.
3. Expand actual-libc guest probes for `gettimeofday`, `clock_getres`, both
   coarse clocks, and counter-disabled fallback. The existing freestanding
   resolver probe is not a substitute for a libc wrapper witness.

## Verification

- RED: `cargo test -p tx-vdso coarse_counter_fallback_uses_vvar_clock_mode_gate --lib`
  failed because the coarse path did not read `VVAR_CLOCK_MODE_OFFSET`.
- GREEN: the same command passed after adding the mode gate.
- `VdsoLayout` has a focused low-`USER_TOP` VM test. Its cargo test is currently
  blocked before `tx-subsystems` compiles by unrelated reactor references to
  deleted timer APIs; the exact errors are recorded in the status update.
- `TX_VDSO_AS=$HOME/.local/bin/riscv64-linux-musl-as cargo test -p tx-vdso --lib`
  passed: 9 tests.
- `git diff --check -- crates/tx-vdso/src/vdso.S crates/tx-vdso/src/tests.rs`
  passed.
