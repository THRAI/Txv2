# Build Toolchain Process Audit

**Date:** 2026-07-15
**Scope:** Read-only fanout of `xtask` command orchestration, CI gates, and
artifact/toolchain paths. No build behavior changed.

## Findings

1. **Artifact selection is not one contract.** Cargo subprocesses inherit
   `CARGO_TARGET_DIR`, while `TxTarget::kernel_path_for_profile` and image/QEMU
   paths resolve under the workspace `target/` directory. A build can therefore
   produce a fresh kernel outside the path QEMU later launches. The Makefile
   already documents this hazard for SMP smoke. Sources:
   `xtask/src/util.rs`, `xtask/src/target.rs`, and `Makefile`.

2. **Preparation is wider than the requested target and some slow lanes rebuild
   the same target.** `full-build --target rv64-qemu` invokes a global doctor
   which requires both RV64 and LA64 tools/targets. `ci-slow` builds RV64, then
   its BusyBox test lane builds RV64 again. The current test image differs from
   `full-build`'s BusyBox CPIO, so the fix must use an image-kind-aware prepare
   contract rather than making `test` call `full-build` blindly. Sources:
   `xtask/src/full_build.rs`, `xtask/src/doctor.rs`, `xtask/src/ci.rs`, and
   `xtask/src/test.rs`.

3. **CI does not prove every gate it reports.** The GitHub slow job does not
   provide `TX_BUSYBOX` or fetch the ignored BusyBox binary. `ci-slow` therefore
   treats the BusyBox boot witness as optional in a fresh runner. The active CI
   reporting document also omits several checks currently run by `ci`, while
   syscall-status is checked twice. Sources: `.github/workflows/check.yml`,
   `xtask/src/ci.rs`, `xtask/src/image.rs`, and
   `docs/design/00_meta-framework/CI_REPORTING_v1.md`.

4. **Artifacts are fixed names rather than run identities.** Image and serial
   paths are keyed only by target/profile. Concurrent or repeated lanes can
   overwrite an image or erase the prior serial log, making failures less
   diagnosable. Sources: `xtask/src/image.rs` and `xtask/src/qemu.rs`.

5. **Reproducibility boundaries need to extend beyond Rust.** The Rust channel
   and both Cargo lockfiles are pinned, but internal Cargo commands do not
   uniformly pass `--locked`; external Docker/package/download inputs are not
   all digest-locked. Sources: `rust-toolchain.toml`, `Cargo.lock`,
   `tools/tx-trace-daemon/Cargo.lock`, `xtask/src/check_build.rs`, and
   `tools/images/fetch-*.sh`.

## Recommended Order

### P0: Correctness before speed

1. Introduce one `ArtifactPaths`/artifact-manifest contract used by build,
   image, QEMU, test, and OSComp. It must resolve `CARGO_TARGET_DIR` once and
   carry the exact kernel/image paths into run commands. A short-term guard may
   reject non-default target directories until all readers use the contract.
2. Make the BusyBox source an explicit, verified CI input and require its boot
   witness when that CI job is required. Otherwise split it into an explicitly
   optional job so green CI never claims coverage it skipped.
3. Split `doctor` into target-scoped requirements plus an explicit `--all`
   clone-wide check. `full-build` should validate only the target(s) selected.

### P1: Remove repeated work without hiding stale inputs

4. Define an internal lane-aware `prepare` primitive: target-scoped preflight,
   kernel build, and declared image kind. Add `test --skip-build` only with an
   explicit artifact/freshness manifest check. Use it for the second RV64 build
   in `ci-slow`.
5. Fingerprint rootfs/image inputs (guest binary digest, overlays, scripts,
   target/profile, environment-injected helpers). Reuse a matching image and
   provide `--force-image`; do not use timestamps as freshness truth.
6. Put serial logs and generated images below `target/runs/<run-id>/`, retain a
   failure bundle, and expose an optional `latest` alias for interactive use.
   QEMU should consume the manifest rather than infer fixed paths.

### P2: Make the reported contract auditable

7. Extract the CI gate list into a single typed manifest used for `check`,
   `ci`, docs validation, and an optional `cargo xtask ci --json` renderer.
   Preserve `check` fail-fast behavior and `ci` aggregate reporting as policies
   over the same gates.
8. Pass `--locked` through all internal Cargo invocations, pin/cache host tool
   inputs deliberately, set GitHub job timeouts and superseded-run concurrency,
   and upload QEMU serial logs on failure. A later `toolchain-inputs.lock` can
   pin Docker bases, guest downloads, and checksums.

## Execution Boundary

This audit does not authorize a broad xtask rewrite. The first implementation
plan should make `ArtifactPaths` and target-scoped doctor independently
testable, add a regression using a non-default target directory, and preserve
the existing smoke versus `test-init` image distinction. Only then should it
introduce image fingerprints or CI workflow changes.

## Verification

- Read-only source and workflow inspection with three independent fanout
  readers.
- Key claims spot-checked against `xtask/src/{full_build,doctor,ci,test,target,
  image,qemu,util}.rs`, `.github/workflows/check.yml`, and
  `docs/design/00_meta-framework/CI_REPORTING_v1.md`.
- `cargo xtask lint docs` passed; it reported the existing seven
  retired-vocabulary mentions as warnings. Scoped `git diff --check` passed for
  the edited status entry, and the new research file has no whitespace errors.

## Next Step and Blockers

Next: accept or narrow the P0 boundary, then create a staged implementation
plan and isolate its files from the current dirty checkout. Blocker: the shared
checkout contains unrelated in-flight changes, including the `xtask` and
progress surfaces; no implementation should start here without an explicit
worktree/scope decision.
