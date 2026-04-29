# CI Reporting Contract — v1

<!-- txdoc:CI-REPORTING-V1 -->

**Status.** Active infrastructure contract.

**Purpose.** Define how automated checks report results, how failures point
back to architecture intent, and how design docs expose grep-stable references.

## 1. Output Shape

<!-- txdoc:CI-REPORT-1 -->

CI output should optimize for quick scan first and detail second:

- passing checks print one concise line with a check mark, check name, and design
  reference;
- skipped checks print one concise line with skip reason and design reference;
- failed checks print command, status, design reference, and bounded captured
  output;
- the final summary counts passed, skipped, and failed checks.

## 2. Design References

<!-- txdoc:CI-DOCREF-1 -->

Design references use HTML-comment tags that do not affect Markdown rendering:

```md
&lt;!-- txdoc:AREA-SUBJECT-1 --&gt;
```

Tags must be stable, uppercase, grep-friendly, and unique across active design
docs. Preferred shape:

```text
txdoc:<AREA>-<SUBJECT>-<NUMBER>
```

Every active design doc must have a file-level tag near the title and at least
one more specific section tag below it. Load-bearing sections should add their
own tags when they are likely to be cited by CI, implementation plans, review,
or agent handoffs.

## 3. Required Gates

<!-- txdoc:CI-GATE-1 -->

The default CI gate is `cargo xtask ci`. It reports these checks individually:

<!-- txdoc:CI-GATE-FMT -->

- `cargo fmt --check`

<!-- txdoc:CI-GATE-CLIPPY -->

- `cargo clippy --workspace --all-targets ... -D warnings`

<!-- txdoc:CI-GATE-HOST-CHECK -->

- `cargo check --workspace`

<!-- txdoc:CI-GATE-UNIT-TESTS -->

- `cargo test --workspace` for host-runnable crates, excluding freestanding
  board binaries that require target-specific harnesses.

<!-- txdoc:CI-GATE-ARCH-LINT -->

- `cargo xtask lint arch`

<!-- txdoc:CI-GATE-DOC-LINT -->

- `cargo xtask lint docs`

<!-- txdoc:CI-GATE-UNUSED-LINT -->

- `cargo xtask lint unused`

<!-- txdoc:CI-GATE-PROGRESS-JSON -->

- `cargo xtask progress validate`

<!-- txdoc:CI-GATE-RV64 -->

- `cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`

<!-- txdoc:CI-GATE-M1DOCK-MOCK -->

- `cargo check -p tx-kernel-riscv64-m1dock-mock --target riscv64gc-unknown-none-elf`

<!-- txdoc:CI-GATE-LA64 -->

- `cargo check -p tx-kernel-loongarch64-qemu-virt --target <available LA64 target>`;
  this check may be skipped when the local rustup toolchain does not provide the
  target.

## 4. Slow Boot Gates

<!-- txdoc:CI-GATE-QEMU-SMOKE -->

The slow CI gate is `cargo xtask ci-slow`. It runs after the fast gate and may
use emulators. Its first required check is:

- `cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel`,
  which passes only after the serial log contains
  `txkernel:qemu-riscv64-virt:boot:ok`.

## 5. Future Boot Gates

<!-- txdoc:CI-BOOT-1 -->

BusyBox, initramfs, filesystem, and OSComp sentinels should be added as the
corresponding implementation milestones land.

## 6. Feature Test Accumulation

<!-- txdoc:CI-TEST-ACCUMULATION-1 -->

Every implemented feature should add the nearest useful test or check while it
is implemented. Plans should name those checks in their JSON `verification`
array before the work is considered complete. Preferred placement:

- pure logic: Rust unit tests in the owning crate;
- architecture boundary: `xtask` lint fixture or compile check;
- board/platform wiring: target compile check and QEMU dry-run contract;
- driver protocol: host-testable mock transport tests;
- boot path: QEMU timeout plus serial sentinel.
