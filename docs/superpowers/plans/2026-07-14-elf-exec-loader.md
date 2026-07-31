# ELF Exec Loader Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace production goblin parsing with a replaceable `ElfFileParser` backed by `elf 0.8.0`, then complete bounded ELF64 RV64/LA64 exec loading through staged reads, platform policy, interpreter layout, and Linux-facing errors.

**Architecture:** Pure syntax decoding lives behind a txKernel-owned trait and returns txKernel-owned header/program-header values. A separate policy/builder validates platform and Linux exec semantics, while `image_reader` owns yield-aware `PageContainer` reads. VM, auxv, and PoNR consume only immutable image plans.

**Tech Stack:** Rust 2021 `no_std`, `elf 0.8.0` with default features disabled, test-only `goblin 0.10.5`, txKernel `StepOutcome`, PageBacked targeted reads, host unit/property tests, RV64/LA64 QEMU witnesses.

---

## Coordination Boundaries

The checkout already contains uncommitted exec `StepOp` migration changes and
an overlapping `2026-07-14-vdso-migration` plan. Preserve those edits. Tasks
1-4 own only `crates/tx-scripts/Cargo.toml`, `Cargo.lock`, `loader.rs`, and
`loader/*`. Tasks 5-7 may edit `script.rs`, `stack.rs`, VM, or vDSO-facing code
only after re-reading the current diff and the vDSO plan state.

The existing `script.rs` `ExecScriptOp`, post-commit StepOps, time imports, and
`exe_file` regression are not part of this migration and must survive intact.

## Target File Topology

| File | Responsibility |
|---|---|
| `crates/tx-scripts/src/process/exec/loader/model.rs` | txKernel-owned decoded ELF values and plan metadata |
| `crates/tx-scripts/src/process/exec/loader/parser.rs` | replaceable `ElfFileParser` trait |
| `crates/tx-scripts/src/process/exec/loader/elf08.rs` | production `elf 0.8.0` decoder |
| `crates/tx-scripts/src/process/exec/loader/policy.rs` | target ISA, ABI, file range, address and segment policy |
| `crates/tx-scripts/src/process/exec/loader.rs` | stable facade and `ExecImagePlan` builder |
| `crates/tx-scripts/src/process/exec/image_reader.rs` | staged PageContainer header/phdr/interpreter reads |
| `crates/tx-scripts/src/process/exec/script.rs` | main/interpreter orchestration, layout and errno mapping |
| `crates/tx-scripts/src/process/exec/stack.rs` | auxv values from the final combined layout |

### Task 1: Add tx-owned decode values and the replaceable parser trait

**Files:**
- Create: `crates/tx-scripts/src/process/exec/loader/model.rs`
- Create: `crates/tx-scripts/src/process/exec/loader/parser.rs`
- Modify: `crates/tx-scripts/src/process/exec/loader.rs`
- Test: `crates/tx-scripts/src/process/exec/loader/tests.rs`

- [ ] **Step 1: Add a compile-failing trait contract test.**

  Add a test-only parser that proves consumers depend only on tx-owned values:

  ```rust
  struct FixtureParser;

  impl ElfFileParser for FixtureParser {
      fn parse_header(_: &[u8]) -> Result<ElfHeader, ElfDecodeError> {
          Ok(ElfHeader::elf64_le(ET_EXEC, EM_RISCV, 0x10080, 64, 56, 2, 0))
      }

      fn parse_program_headers(
          _: &ElfHeader,
          _: &[u8],
      ) -> Result<Vec<ElfProgramHeader>, ElfDecodeError> {
          Ok(Vec::new())
      }
  }

  #[test]
  fn parser_trait_exposes_only_tx_owned_values() {
      let header = FixtureParser::parse_header(&[]).unwrap();
      assert_eq!(header.entry, 0x10080);
  }
  ```

- [ ] **Step 2: Run RED.**

  Run: `cargo test -p tx-scripts parser_trait_exposes_only_tx_owned_values --lib`

  Expected: compile failure because `ElfFileParser`, `ElfHeader`,
  `ElfProgramHeader`, and `ElfDecodeError` do not exist.

- [ ] **Step 3: Add the minimal model and trait.**

  Define copyable `ElfClass`, `ElfEndian`, `ElfHeader`, and
  `ElfProgramHeader` values containing the standard ELF fields. Define:

  ```rust
  #[derive(Clone, Copy, Debug, Eq, PartialEq)]
  pub enum ElfDecodeError {
      Truncated,
      BadMagic,
      UnsupportedClass,
      UnsupportedEndian,
      UnsupportedVersion,
      BadEntrySize,
      IntegerOverflow,
      Malformed,
  }

  pub trait ElfFileParser {
      fn parse_header(bytes: &[u8]) -> Result<ElfHeader, ElfDecodeError>;
      fn parse_program_headers(
          header: &ElfHeader,
          bytes: &[u8],
      ) -> Result<Vec<ElfProgramHeader>, ElfDecodeError>;
  }
  ```

  Re-export these values through `loader.rs`. Do not import goblin or elf in
  either model or trait module.

- [ ] **Step 4: Run GREEN and the existing loader tests.**

  Run:

  ```sh
  cargo test -p tx-scripts parser_trait_exposes_only_tx_owned_values --lib
  cargo test -p tx-scripts process::exec::loader::tests --lib
  ```

  Expected: contract test passes; existing 23 loader tests remain green.

- [ ] **Step 5: Commit the bounded trait slice.**

  ```sh
  git add crates/tx-scripts/src/process/exec/loader.rs crates/tx-scripts/src/process/exec/loader/model.rs crates/tx-scripts/src/process/exec/loader/parser.rs crates/tx-scripts/src/process/exec/loader/tests.rs
  git commit -m "feat(exec): add replaceable ELF parser contract"
  ```

### Task 2: Implement `Elf08Parser` and differential syntax tests

**Files:**
- Modify: `crates/tx-scripts/Cargo.toml`
- Modify: `Cargo.lock`
- Create: `crates/tx-scripts/src/process/exec/loader/elf08.rs`
- Modify: `crates/tx-scripts/src/process/exec/loader.rs`
- Test: `crates/tx-scripts/src/process/exec/loader/tests.rs`

- [ ] **Step 1: Add failing ELF 0.8 decoder tests.**

  Reuse the existing fixture builder and assert exact header and program
  fields:

  ```rust
  #[test]
  fn elf08_parser_decodes_header_and_independent_phdr_table() {
      let bytes = FixtureCfg::minimal().build();
      let header = Elf08Parser::parse_header(&bytes[..64]).unwrap();
      let table_len = header.phnum as usize * header.phentsize as usize;
      let table = &bytes[header.phoff as usize..header.phoff as usize + table_len];
      let phdrs = Elf08Parser::parse_program_headers(&header, table).unwrap();
      assert_eq!(header.class, ElfClass::Elf64);
      assert_eq!(header.endian, ElfEndian::Little);
      assert_eq!(phdrs.len(), 2);
      assert_eq!(phdrs[1].p_type, PT_LOAD);
  }
  ```

  Add separate truncation and big-endian rejection tests.

- [ ] **Step 2: Run RED.**

  Run: `cargo test -p tx-scripts elf08_parser_ --lib`

  Expected: compile failure because `Elf08Parser` and the elf dependency do
  not exist.

- [ ] **Step 3: Add the production dependency and decoder.**

  In `[dependencies]` add:

  ```toml
  elf = { version = "0.8.0", default-features = false }
  ```

  Implement header parsing with
  `parse_ident::<elf::endian::LittleEndian>` and
  `FileHeader::parse_tail`. Implement program-header parsing with
  `ParsingTable::<LittleEndian, elf::segment::ProgramHeader>::new`. Convert
  every result immediately into tx-owned values and map `elf::ParseError`
  into stable `ElfDecodeError` variants.

- [ ] **Step 4: Run GREEN plus no-default-feature check.**

  Run:

  ```sh
  cargo test -p tx-scripts elf08_parser_ --lib
  cargo check -p tx-scripts --lib
  ```

  Expected: decoder tests pass and the no_std crate compiles without elf's
  `alloc`, `std`, or `to_str` features.

- [ ] **Step 5: Commit the decoder slice.**

  ```sh
  git add Cargo.lock crates/tx-scripts/Cargo.toml crates/tx-scripts/src/process/exec/loader.rs crates/tx-scripts/src/process/exec/loader/elf08.rs crates/tx-scripts/src/process/exec/loader/tests.rs
  git commit -m "feat(exec): decode ELF files with elf 0.8"
  ```

### Task 3: Move image-plan construction onto parser-independent facts

**Files:**
- Create: `crates/tx-scripts/src/process/exec/loader/policy.rs`
- Modify: `crates/tx-scripts/src/process/exec/loader.rs`
- Test: `crates/tx-scripts/src/process/exec/loader/tests.rs`

- [ ] **Step 1: Add RED tests for backend equivalence and malformed ranges.**

  Add `parse_image_plan_with::<Elf08Parser>(&bytes, policy)` coverage for the
  existing fixture set. Add explicit tests for `e_version != EV_CURRENT`,
  `p_offset + p_filesz` overflow, entry outside executable LOAD, invalid
  `PT_PHDR`, and W+X LOAD acceptance.

- [ ] **Step 2: Run RED.**

  Run: `cargo test -p tx-scripts parse_image_plan_with_ --lib`

  Expected: compile failure because the generic plan builder and policy do not
  exist, followed by behavioral failures for newly required policy.

- [ ] **Step 3: Introduce `ElfLoadPolicy` and generic plan construction.**

  Define:

  ```rust
  pub struct ElfLoadPolicy {
      pub arch: tx_hal::Arch,
      pub page_size: u64,
      pub user_top: u64,
      pub max_phdrs: u16,
      pub allow_interpreter: bool,
  }

  impl ElfLoadPolicy {
      pub fn for_platform<P: tx_hal::PlatformConfig>() -> Self;
      pub const fn fixture(arch: tx_hal::Arch, user_top: u64) -> Self;
  }
  ```

  Parse through `ElfFileParser`, validate policy using only tx-owned values,
  and retain `parse_image_plan` as an `Elf08Parser` compatibility facade.
  Use checked arithmetic for every file and final virtual range. Require the
  final entry to lie inside an executable LOAD and below `user_top`.

- [ ] **Step 4: Run GREEN and all loader tests.**

  Run: `cargo test -p tx-scripts process::exec::loader::tests --lib`

  Expected: all old fixtures plus new policy regressions pass. Update old
  assertions only where the approved Linux-compatible policy intentionally
  changed (for example W+X acceptance).

- [ ] **Step 5: Commit policy conversion.**

  ```sh
  git add crates/tx-scripts/src/process/exec/loader.rs crates/tx-scripts/src/process/exec/loader/policy.rs crates/tx-scripts/src/process/exec/loader/tests.rs
  git commit -m "feat(exec): validate ELF plans with platform policy"
  ```

### Task 4: Record complete exec-relevant program-header metadata

**Files:**
- Modify: `crates/tx-scripts/src/process/exec/loader/model.rs`
- Modify: `crates/tx-scripts/src/process/exec/loader.rs`
- Test: `crates/tx-scripts/src/process/exec/loader/tests.rs`

- [ ] **Step 1: Add RED fixtures for TLS, RELRO, dynamic and GNU stack.**

  Build one fixture per header and assert `ExecImagePlan` carries:

  ```rust
  assert_eq!(plan.tls.unwrap().file_size, 16);
  assert_eq!(plan.relro.unwrap().size, 4096);
  assert!(plan.dynamic.is_some());
  assert!(plan.stack.executable_requested);
  ```

  Add duplicate `PT_INTERP`, duplicate `PT_TLS`, and malformed metadata-range
  rejection tests.

- [ ] **Step 2: Run RED.**

  Run: `cargo test -p tx-scripts parse_image_plan_records_ --lib`

  Expected: compile failures for missing plan fields.

- [ ] **Step 3: Add typed plan metadata.**

  Define `TlsTemplate`, `ImageRange`, `StackRequest`, and optional dynamic and
  RELRO facts using checked final addresses. Keep dynamic tags, symbols, and
  relocation parsing out of the kernel. Require at most one `PT_INTERP` and
  `PT_TLS`; record one GNU stack request and default to NX-requested when the
  header is absent.

- [ ] **Step 4: Run GREEN.**

  Run: `cargo test -p tx-scripts process::exec::loader::tests --lib`

  Expected: metadata and existing mapping tests pass.

- [ ] **Step 5: Commit metadata completion.**

  ```sh
  git add crates/tx-scripts/src/process/exec/loader.rs crates/tx-scripts/src/process/exec/loader/model.rs crates/tx-scripts/src/process/exec/loader/tests.rs
  git commit -m "feat(exec): retain ELF runtime segment metadata"
  ```

### Task 5: Add staged main/interpreter image reads

**Files:**
- Create: `crates/tx-scripts/src/process/exec/image_reader.rs`
- Modify: `crates/tx-scripts/src/process/exec/mod.rs`
- Modify: `crates/tx-scripts/src/process/exec/script.rs`
- Test: `crates/tx-scripts/src/process/exec/script/tests.rs`
- Test: `crates/tx-scripts/src/process/exec/loader/tests.rs`

- [ ] **Step 1: Re-read the live vDSO plan and current exec diff.**

  Run:

  ```sh
  git diff -- crates/tx-scripts/src/process/exec
  sed -n '1,180p' docs/progress/plans/2026-07-14-vdso-migration.json
  ```

  Expected: preserve `ExecScriptOp`, post-commit StepOps, and any newly landed
  vDSO auxv integration. If phase 4 is actively editing the same lines, stop
  and coordinate rather than overwriting it.

- [ ] **Step 2: Add RED staged-read tests.**

  Add a fixture with `e_phoff = 8192` and a valid table there; add a dynamic
  fixture whose `PT_INTERP` string is beyond the first page; add an extreme
  `p_offset` fixture and assert `ENOEXEC` without panic.

- [ ] **Step 3: Run RED.**

  Run: `cargo test -p tx-scripts staged_elf_read_ --lib`

  Expected: the valid out-of-window fixtures fail under the current one-window
  parser.

- [ ] **Step 4: Implement the staged reader.**

  `image_reader` reads 64 bytes, asks `Elf08Parser` for the header, checks the
  bounded table request, reads that exact table, parses it, computes an
  optional interpreter-string request, then reads and validates the string.
  Main and interpreter call the same helper with distinct `ImageRole` values.
  Return typed read/policy errors; never perform unchecked `u64 -> usize`
  conversion or `off + len` arithmetic.

- [ ] **Step 5: Wire production exec and run GREEN.**

  Replace both 4 KiB parse windows in `script.rs` with the staged helper. Keep
  all reads pre-PoNR and preserve current `StepOutcome` handling until the
  overlapping StepOp migration provides the multi-step continuation.

  Run:

  ```sh
  cargo test -p tx-scripts staged_elf_read_ --lib
  cargo test -p tx-scripts process::exec --lib
  ```

  Expected: out-of-window main/interpreter fixtures pass and existing exec
  script behavior remains green.

- [ ] **Step 6: Commit staged reads.**

  ```sh
  git add crates/tx-scripts/src/process/exec/image_reader.rs crates/tx-scripts/src/process/exec/mod.rs crates/tx-scripts/src/process/exec/script.rs crates/tx-scripts/src/process/exec/script/tests.rs crates/tx-scripts/src/process/exec/loader/tests.rs
  git commit -m "feat(exec): read ELF metadata in bounded stages"
  ```

### Task 6: Converge interpreter layout, auxv and errors

**Files:**
- Modify: `crates/tx-scripts/src/process/exec/script.rs`
- Modify: `crates/tx-scripts/src/process/exec/stack.rs`
- Modify: `crates/tx-subsystems/src/vm/scripts.rs`
- Test: `crates/tx-scripts/src/process/exec/script/tests.rs`

- [ ] **Step 1: Add RED combined-layout tests.**

  Assert dynamic exec uses interpreter PC, main `AT_ENTRY`, interpreter
  `AT_BASE`, main `AT_PHDR`, and non-overlapping main/interpreter/stack ranges.
  Add malformed and nested interpreter tests with `ELIBBAD`-typed internal
  errors.

- [ ] **Step 2: Run RED.**

  Run: `cargo test -p tx-scripts dynamic_exec_layout_ --lib`

  Expected: current ad-hoc interpreter adjustments fail at least one typed
  layout/error assertion.

- [ ] **Step 3: Add one checked layout selection pass.**

  Choose randomized page-aligned main, interpreter, stack, and available vDSO
  ranges against `P::USER_TOP`; retry a bounded number of entropy candidates;
  return `ElfLayoutError` if no non-overlapping layout exists. Apply deltas
  with checked unsigned arithmetic, never `as i64` round-trips.

- [ ] **Step 4: Remove core-loader interpreter path fallbacks.**

  Resolve the exact absolute `PT_INTERP` path through process root/mount
  namespace. Preserve OSComp compatibility only through an explicitly named
  adapter or image symlink owned outside ELF parsing.

- [ ] **Step 5: Run GREEN.**

  Run:

  ```sh
  cargo test -p tx-scripts dynamic_exec_layout_ --lib
  cargo test -p tx-scripts process::exec --lib
  cargo check -p tx-subsystems --lib --no-default-features
  ```

  Expected: layout, auxv, parser and VM integration tests pass.

- [ ] **Step 6: Commit layout convergence.**

  ```sh
  git add crates/tx-scripts/src/process/exec/script.rs crates/tx-scripts/src/process/exec/stack.rs crates/tx-scripts/src/process/exec/script/tests.rs crates/tx-subsystems/src/vm/scripts.rs
  git commit -m "feat(exec): validate dynamic ELF layout and auxv"
  ```

### Task 7: Retire goblin and close core verification

**Files:**
- Modify: `crates/tx-scripts/Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `docs/design/02_execution/EXEC_v1.md`
- Modify: `docs/progress/STATUS.md`
- Modify: `docs/progress/research/2026-07-14-elf-parser-library-audit.md`
- Test: `crates/tx-scripts/src/process/exec/loader/tests.rs`

- [ ] **Step 1: Move goblin to test-only differential coverage.**

  First keep `GoblinParser` behind `#[cfg(test)]` and compare it with
  `Elf08Parser` on existing real/hand-built fixtures. Document intentional
  differences in test names. Then delete the test backend and remove goblin
  entirely after the corpus agrees on syntax facts.

- [ ] **Step 2: Verify production metadata contains no goblin.**

  Run:

  ```sh
  cargo tree -p tx-scripts -e normal
  rg -n 'goblin' crates/tx-scripts/Cargo.toml Cargo.lock crates/tx-scripts/src/process/exec
  ```

  Expected: `cargo tree` has `elf v0.8.0` and no goblin; `rg` finds only
  historical progress/design text if intentionally retained.

- [ ] **Step 3: Update the active exec contract.**

  Rewrite `EXEC_v1` sections 8.2-8.10 and 9.4 to describe staged reads,
  dynamic interpreter support, platform policy, TLS/RELRO/GNU-stack facts,
  and current vDSO/executable-lease dependencies. Add grep-stable tags for the
  parser trait and staged-reader contracts.

- [ ] **Step 4: Run the core gate ladder.**

  Run:

  ```sh
  cargo test -p tx-scripts process::exec --lib
  cargo check -p tx-scripts --lib
  cargo -q xtask unit
  cargo xtask lint docs
  cargo xtask progress validate
  git diff --check -- crates/tx-scripts crates/tx-subsystems/src/vm/scripts.rs docs/design/02_execution/EXEC_v1.md docs/progress
  ```

  Expected: focused tests and checks pass. If progress validation is still
  blocked only by the pre-existing invalid `vdso-migration` status value,
  record that exact external blocker without editing the other plan.

- [ ] **Step 5: Run guest witnesses after core gates.**

  Run RV64 and LA64 QEMU exec witnesses covering static, static PIE, musl
  dynamic and glibc dynamic images, followed by the relevant LTP `execve*`
  and OSComp/libctest groups. Preserve serial logs and exact case markers.

- [ ] **Step 6: Commit the core closeout.**

  ```sh
  git add Cargo.lock crates/tx-scripts/Cargo.toml crates/tx-scripts/src/process/exec docs/design/02_execution/EXEC_v1.md docs/progress/STATUS.md docs/progress/research/2026-07-14-elf-parser-library-audit.md
  git commit -m "feat(exec): replace goblin with complete ELF loader"
  ```

## Deferred Cross-Plan Closure

The following are required for the design's final completion statement but
are coordinated through their owning plans rather than edited opportunistically
inside Tasks 1-7:

- executable lease / `ETXTBSY` and generation pinning: PageBacked/VFS owner;
- NX stack enforcement: existing vDSO migration phase 6 supplies
  `rt_sigreturn`, after which exec consumes `StackRequest`;
- fully yielding multi-step exec reads: current `ExecScriptOp` migration must
  replace the one-shot `EBUSY` collapse with retained read continuation.

Until those owners close their gates, report the parser/core loader as landed
and the complete Linux exec-loader program as active, not complete.
