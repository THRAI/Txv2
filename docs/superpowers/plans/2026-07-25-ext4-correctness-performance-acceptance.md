# ext4 Correctness and Performance Acceptance Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> `superpowers:subagent-driven-development` (recommended) or
> `superpowers:executing-plans` to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prove the repaired PageBacked/ext4/I/O path against pinned Linux,
e2fsprogs and xfstests authorities, then measure a native rustc kernel build on
one CPU, 4 GiB RAM and no swap with enough attribution to explain every missed
performance gate.

**Architecture:** `cargo xtask ext4` owns reproducible host orchestration and
result schemas. QEMU exposes stable TEST, SCRATCH and optional WORKLOAD block
roles; txKernel registers each role as an independent virtio-blk device. A
read-only seed is copied for every destructive run. Guest scripts execute
xfstests and the compiler workload, while host fault orchestration preserves
crash images and applies Linux/e2fsprogs oracles offline. tx-observe supplies
monotonic counters and wait/critical-section attribution; it does not control
filesystem behavior.

**Tech Stack:** Rust xtask, RV64 QEMU virtio-mmio, Alpine Linux, Bash,
e2fsprogs, xfsprogs, xfstests, Python 3 standard library, tx-observe, JBD2,
Linux ext4 and native RV64 rustc/cargo.

---

## Authority and non-negotiable rules

- Linux ext4 defines visible behavior; e2fsprogs defines image structure and
  offline consistency; the selected xfstests revision defines regression
  cases. Tx self-checks and rsext4 tests cannot override them.
- Canonical fixture and workload images are immutable. Every QEMU or host
  mutation uses a fresh raw copy under `target/ext4/runs/<run-id>/`.
- TEST and SCRATCH are different images and different virtio devices. A test
  run that aliases them, cannot obtain an exclusive host lock, or cannot prove
  their device identities fails before mounting either device.
- Tier 1 is a permanent fast gate. Tier 2 adds feature slices and tests; it may
  not remove Tier 1 cases, widen Tier 1 feature admission, or turn a Tier 1
  failure into an exclusion.
- A crash campaign records the exact cut point and occurrence. Randomized
  order may supplement deterministic coverage but cannot replace it.
- `e2fsck -fn` always runs on an offline copy. Linux replay and RW mount tests
  run on separate copies, never on the preserved crash artifact.
- Timed Linux and Tx runs use the same QEMU CPU count, RAM, disk seeds, QEMU
  drive settings, source, toolchain and workload command. Results from
  different hosts or fixture hashes are not combined.
- Normal aligned file payload requires zero extra copies and zero bounce
  bytes. Journal encoding, immutable metadata freezing and checkpoint bytes
  are reported separately rather than hidden in payload accounting.

## File structure

- Create `xtask/src/ext4.rs` plus `xtask/src/ext4/` modules for command parsing,
  profiles, fixtures, fault runs, xfstests, performance and reports.
- Modify `xtask/src/lib.rs`, `xtask/src/qemu.rs` and
  `xtask/src/shell_test.rs` for the new command and stable block roles.
- Modify RV64 HAL/device initialization so `virtio0` through `virtio2` may be
  registered independently as `vda`, `vdb` and `vdc`.
- Create `tools/ext4/profiles/`, `tools/ext4/guest/` and host-only harnesses.
- Create `tools/ext4/workloads/` for the pinned native rustc kernel-build
  workload and its image recipe.
- Modify tx-observe schema and owner callsites for copy, I/O, reclaim,
  writeback, refault, pager and queue attribution.
- Store generated images and raw results only under `target/ext4/`; store a
  reviewed promotion report under `docs/progress/research/`.

### Task 1: Add the versioned `cargo xtask ext4` orchestration surface

**Files:**
- Create: `xtask/src/ext4.rs`
- Create: `xtask/src/ext4/profile.rs`
- Create: `xtask/src/ext4/report.rs`
- Modify: `xtask/src/lib.rs`
- Test: `xtask/src/ext4.rs`
- Test: `xtask/src/ext4/profile.rs`

- [ ] **Step 1: Add failing command and schema tests**

Add parser tests for these exact entry points:

```text
cargo xtask ext4 fixture generate --profile PATH --out DIR
cargo xtask ext4 fixture verify --profile PATH --fixtures DIR
cargo xtask ext4 fault --profile PATH --case NAME --cut POINT --iterations N
cargo xtask ext4 xfstests --manifest PATH --test-image PATH --scratch-image PATH
cargo xtask ext4 perf prepare --manifest PATH --out DIR
cargo xtask ext4 perf run --manifest PATH --kernel linux|tx --lane raw|tmpfs|ext4
cargo xtask ext4 report --inputs DIR --out DIR --gate bringup|product
```

The tests must reject unknown fields, relative paths that escape the workspace,
non-positive iteration counts, duplicate test IDs, TEST/SCRATCH path equality,
mutable seed images, missing full-length source revisions, missing SHA-256
values and a report that mixes host IDs or fixture hashes. `--dry-run` renders
the complete child commands and resolved paths without creating images.

- [ ] **Step 2: Run the RED tests**

```sh
cargo test -p xtask ext4 -- --test-threads=1
cargo xtask ext4 --help
```

Expected: compilation or command dispatch fails because the `ext4` module and
subcommand do not exist.

- [ ] **Step 3: Implement typed profiles and atomic result writing**

Use `serde` DTOs with `deny_unknown_fields`. Every profile carries
`schema_version`, full source revisions, tool version output, artifact hashes,
QEMU geometry and an explicit case list. Every run gets a collision-resistant
run ID, `run.json`, serial log, resolved manifest copy, image hashes, start/end
timestamps and terminal status. Write results to a temporary sibling and
rename only after every required artifact is closed; an interrupted run stays
marked `incomplete` and cannot be promoted.

Do not silently download tools. `fixture generate` and `perf prepare` consume
explicit source/archive paths or a repository-managed mirror configuration and
record their hashes. Missing inputs produce an actionable error.

- [ ] **Step 4: Verify parser and dry-run behavior**

```sh
cargo test -p xtask ext4 -- --test-threads=1
cargo xtask ext4 fixture verify --profile tools/ext4/profiles/tier1.json --fixtures target/ext4/fixtures --dry-run
cargo xtask --help
git diff --check
```

Expected: unit tests pass; dry-run resolves commands without requiring the
not-yet-generated fixture directory; top-level help lists every ext4 verb.

- [ ] **Step 5: Commit**

```sh
git add xtask/src/ext4.rs xtask/src/ext4 xtask/src/lib.rs
git commit -m "feat(xtask): add ext4 acceptance command surface"
```

### Task 2: Expose independent TEST, SCRATCH and WORKLOAD block roles

**Files:**
- Modify: `xtask/src/qemu.rs`
- Modify: `xtask/src/shell_test.rs`
- Modify: `boards/tx-hal-riscv64-qemu-virt/src/boot_static.rs`
- Modify: `crates/tx-kernel/src/devices.rs`
- Modify: `crates/tx-drivers/src/virtio/mmio.rs`
- Test: `xtask/src/qemu.rs`
- Test: `xtask/src/shell_test.rs`
- Test: `crates/tx-kernel/src/devices.rs`

- [ ] **Step 1: Add failing multi-device command tests**

Replace the internal single `extra_rv64_ext4` value with typed optional roles:

```rust
struct Ext4BlockImages {
    test: Option<PathBuf>,
    scratch: Option<PathBuf>,
    workload: Option<PathBuf>,
}
```

Parse `--ext4-test-image`, `--ext4-scratch-image` and
`--ext4-workload-image`. The old `--extra-rv64-ext4` remains a compatibility
alias for TEST during this task and is rejected when any new role flag is also
present. Tests require stable QEMU mapping:

| Role | drive ID | virtio-mmio bus | guest device | mode |
|---|---|---|---|---|
| TEST | `tx-test` | `virtio-mmio-bus.0` | `vda` | writable |
| SCRATCH | `tx-scratch` | `virtio-mmio-bus.1` | `vdb` | writable |
| WORKLOAD | `tx-workload` | `virtio-mmio-bus.2` | `vdc` | read-only |

Tests reject duplicate canonical paths and any non-RV64 target. QEMU command
tests assert distinct `-drive` IDs, buses and `read-only=on` only for WORKLOAD.

- [ ] **Step 2: Add failing HAL and registry tests**

Extend the platform-info test to require mapped `virtio0`, `virtio1` and
`virtio2` MMIO pages at `0x1000_1000`, `0x1000_2000` and `0x1000_3000`.
Device tests use three mock block transports and prove deterministic
registration as `DevT(254,0..2)` / `vda..vdc`, while absent transports are
skipped without renumbering a later role. Keep `ltpdev` on its existing
separate identity.

- [ ] **Step 3: Run the RED tests**

```sh
cargo test -p xtask qemu_can_attach_named_ext4_roles -- --test-threads=1
cargo test -p xtask shell_test_can_attach_named_ext4_roles -- --test-threads=1
cargo test -p tx-kernel block_devices --lib -- --test-threads=1
```

Expected: tests fail because only `virtio0` and one `txblk0` path exist.

- [ ] **Step 4: Implement multi-device discovery and registration**

Increase the RV64 static MMIO region capacity and map all three block slots.
Construct one independent `VirtioMmioBlock` per role; never put two transports
behind one lock. Probe the virtio device type before registration so a missing
slot or non-block device cannot shift TEST/SCRATCH names. Append
`tx.ext4.test=vda tx.ext4.scratch=vdb tx.ext4.workload=vdc` to harness command
lines and have guest scripts verify those names against `/proc/cmdline` before
mounting.

The shell-test parallel path clones role descriptors and gives every worker
fresh writable copies; it must never boot two workers against the same TEST or
SCRATCH file.

- [ ] **Step 5: Verify all builders**

```sh
cargo test -p xtask qemu -- --test-threads=1
cargo test -p xtask shell_test -- --test-threads=1
cargo test -p tx-kernel block_devices --lib -- --test-threads=1
cargo xtask qemu --target rv64-qemu --profile alpine --dry-run --ext4-test-image target/ext4/test.img --ext4-scratch-image target/ext4/scratch.img --ext4-workload-image target/ext4/workload.img
git diff --check
```

Expected: rendered QEMU command contains three distinct block roles; tests
prove stable guest identities even when one optional role is absent.

- [ ] **Step 6: Commit**

```sh
git add xtask/src/qemu.rs xtask/src/shell_test.rs boards/tx-hal-riscv64-qemu-virt/src/boot_static.rs crates/tx-kernel/src/devices.rs crates/tx-drivers/src/virtio/mmio.rs
git commit -m "feat(rv64): expose independent ext4 test block roles"
```

### Task 3: Pin Tier 1/Tier 2 profiles and deterministic fixtures

**Files:**
- Create: `tools/ext4/profiles/schema.json`
- Create: `tools/ext4/profiles/upstream-lock.json`
- Create: `tools/ext4/profiles/tier1.json`
- Create: `tools/ext4/profiles/tier1-xfstests.json`
- Create: `tools/ext4/profiles/tier2.json`
- Create: `tools/ext4/profiles/tier2-xfstests.json`
- Create: `tools/ext4/fixture_matrix.py`
- Create: `tools/tests/test_ext4_fixture_matrix.py`
- Create: `xtask/src/ext4/fixture.rs`
- Modify: `xtask/src/ext4.rs`

- [ ] **Step 1: Add failing manifest and reproducibility tests**

The profile schema must require:

- a full Linux source commit, e2fsprogs commit, xfsprogs commit and xfstests
  commit plus human-readable version output;
- Alpine repository snapshot URLs and SHA-256 values for the APK index and
  every installed package;
- accepted incompat/ro-compat/compat masks, 4-KiB geometry, inode sizes,
  extent-depth/directory-fanout bounds and the pre-mutation rejection rule;
- exact `mke2fs` arguments and normalized `dumpe2fs -h` facts;
- every fixture path, SHA-256, construction script and expected feature facts;
- selected xfstests IDs, ordered execution groups and an exclusion row with
  `authority`, `missing_surface`, `tracking_ref` and `revisit_gate`; and
- promotion evidence paths and the hash of the manifest used to produce them.

The source lock is created from checked-out upstream trees with
`git rev-parse HEAD`; annotated names alone are rejected. The implementation
task is complete only when all revision/hash fields contain measured values and
fixture regeneration produces byte-identical images twice. This avoids baking
an unverified revision guess into the plan.

- [ ] **Step 2: Run the RED tests**

```sh
python3 -m unittest tools.tests.test_ext4_fixture_matrix
cargo test -p xtask ext4::profile -- --test-threads=1
```

Expected: tests fail because profiles and fixture tooling do not exist.

- [ ] **Step 3: Generate immutable profile fixtures**

Implement deterministic construction for clean/dirty journal states, linear
and non-splitting htree directories, inline and depth-1 extents, sparse growth,
fast/block symlinks, hard links, bounded full groups, orphan recovery and every
Tier 2 extension admitted by its manifest. Normalize timestamps, UUIDs, hash
seeds, ownership and directory insertion order. Store generators and small
source payloads in Git; store generated raw images under
`target/ext4/fixtures/<manifest-hash>/`.

For every fixture, retain normalized `dumpe2fs`, `debugfs` and `e2fsck -fn`
outputs. `fixture verify` copies the seed, never opens it writable, verifies the
hash before and after the run, and rejects facts outside the profile.

- [ ] **Step 4: Build the selected-case manifests from the capability ledger**

For Tier 1, select the generic/ext4 tests whose operations and `_require`
conditions are entirely inside the Tier 1 matrix. For Tier 2, add cases only in
the same commit as the feature slice they prove. Each test file is hashed; a
changed upstream test body invalidates old evidence even if its numeric ID is
unchanged. Kernel-wide missing syscalls may be explicit exclusions, but an
ext4 semantic failure, timeout, crash or not-run result is never an exclusion.

- [ ] **Step 5: Verify reproducibility and profile monotonicity**

```sh
python3 -m unittest tools.tests.test_ext4_fixture_matrix
cargo test -p xtask ext4::profile -- --test-threads=1
cargo test -p xtask ext4::fixture -- --test-threads=1
cargo xtask ext4 fixture generate --profile tools/ext4/profiles/tier1.json --out target/ext4/fixtures/run-a
cargo xtask ext4 fixture generate --profile tools/ext4/profiles/tier1.json --out target/ext4/fixtures/run-b
cargo xtask ext4 fixture verify --profile tools/ext4/profiles/tier1.json --fixtures target/ext4/fixtures/run-a
diff -ru target/ext4/fixtures/run-a target/ext4/fixtures/run-b
git diff --check
```

Expected: two generations are byte-identical; Tier 2 is a strict superset of
Tier 1; all canonical seeds remain unchanged.

- [ ] **Step 6: Commit**

```sh
git add tools/ext4/profiles tools/ext4/fixture_matrix.py tools/tests/test_ext4_fixture_matrix.py xtask/src/ext4.rs xtask/src/ext4/fixture.rs
git commit -m "test(ext4): pin compatibility profiles and fixtures"
```

### Task 4: Add deterministic crash, replay and I/O-error campaigns

**Files:**
- Create: `tools/ext4/fault_harness.py`
- Create: `tools/tests/test_ext4_fault_harness.py`
- Create: `xtask/src/ext4/fault.rs`
- Create: `crates/tx-ext4/src/fault.rs`
- Modify: `crates/tx-ext4/src/lib.rs`
- Modify: `crates/tx-ext4/src/journal.rs`
- Modify: `crates/tx-ext4/src/namespace.rs`
- Modify: `crates/tx-subsystems/src/io_manager/backend/graph.rs`
- Modify: `boards/tx-kernel-riscv64-qemu-virt/Cargo.toml`
- Create: `tools/ext4/guest/ext4-fault-cases.sh`

- [ ] **Step 1: Add failing fault-protocol tests**

Define test-build-only one-shot cut points:

```text
before_admission
after_ordered_data
after_journal_descriptor
after_journal_metadata
after_journal_fence
after_commit
after_checkpoint
after_clean_superblock
```

The hook emits exactly one line containing run ID, point, occurrence,
transaction sequence and mutation case, then parks without starting later I/O.
The host kills QEMU only after parsing that complete marker. Production builds
contain no selectable hook; a static test fails if ordinary code can enable it.

Host unit tests use a fake QEMU process and fake e2fs tools to prove marker
parsing, timeout handling, SIGKILL, immutable seed preservation, per-run copies,
exclusive locking, old/new oracle selection and incomplete-result retention.

- [ ] **Step 2: Run the RED tests**

```sh
python3 -m unittest tools.tests.test_ext4_fault_harness
cargo test -p tx-ext4 fault --lib -- --test-threads=1
cargo test -p xtask ext4::fault -- --test-threads=1
```

Expected: tests fail because the cut-point protocol and harness do not exist.

- [ ] **Step 3: Wire typed graph-phase hooks**

Use the durability-domain/node-role values introduced by the transactional
data-plane plan. Hooks observe admission or successful node completion; they
do not add a second scheduler, issue block I/O, own graph resources or alter
completion settlement. `before_admission` fires before ring/allocator state is
published. Pre-commit graph points mean the namespace must replay to the old
state. `after_commit` and later mean the new state must survive. A deliberately
failed commit write after submission is classified durability-unknown and may
produce old or new state, but must force the specified error/RO recovery path.

- [ ] **Step 4: Implement the host oracle sequence**

For `write_fsync`, `create`, `unlink`, `rename_same_dir`,
`rename_cross_dir`, `truncate` and `clean_unmount`, preserve the killed image
and perform on separate copies:

1. offline `e2fsck -fn` and capture complete output/exit status;
2. pinned Linux journal replay by RW mount/unmount;
3. a second offline `e2fsck -fn` that must report clean;
4. `debugfs`/file hash/namespace checks for the required old, new or
   durability-unknown outcome; and
5. Tx remount validation, including remount-RO/quarantine on integrity or
   commit-I/O error.

Add non-crash I/O failures at admission, ordered data, commit submission and
checkpoint completion. Assert exactly-once lease/ring settlement, reservation
reuse only after safe pre-commit abort, quarantine after unknown commit
durability and retained retry state after checkpoint error.

- [ ] **Step 5: Run focused and campaign gates**

```sh
python3 -m unittest tools.tests.test_ext4_fault_harness
cargo test -p tx-ext4 fault journal --lib -- --test-threads=1
cargo test -p tx-subsystems io_manager::backend::graph --lib -- --test-threads=1
cargo xtask ext4 fault --profile tools/ext4/profiles/tier1.json --case all --cut all --iterations 1
cargo xtask ext4 fault --profile tools/ext4/profiles/tier1.json --case all --cut all --iterations 1000
git diff --check
```

Expected: the single-iteration matrix is the PR gate; the 1000-iteration run is
the promotion/nightly gate. No image is repaired in place and no failure lacks
the cut point, serial log, fsck output and namespace/data oracle.

- [ ] **Step 6: Commit**

```sh
git add tools/ext4/fault_harness.py tools/tests/test_ext4_fault_harness.py tools/ext4/guest/ext4-fault-cases.sh xtask/src/ext4/fault.rs crates/tx-ext4/src/fault.rs crates/tx-ext4/src/lib.rs crates/tx-ext4/src/journal.rs crates/tx-ext4/src/namespace.rs crates/tx-subsystems/src/io_manager/backend/graph.rs boards/tx-kernel-riscv64-qemu-virt/Cargo.toml
git commit -m "test(ext4): add deterministic crash and replay campaigns"
```

### Task 5: Build the Alpine xfstests guest and run Tier 1/Tier 2 lanes

**Files:**
- Create: `tools/ext4/build_alpine_xfstests.py`
- Create: `tools/tests/test_ext4_alpine_image.py`
- Create: `tools/ext4/guest/run-xfstests.sh`
- Create: `tools/shell-tests/alpine-ext4-xfstests.txt`
- Create: `xtask/src/ext4/xfstests.rs`
- Modify: `xtask/src/ext4.rs`
- Modify: `xtask/src/doctor.rs`

- [ ] **Step 1: Add failing image and runner tests**

The image recipe installs pinned Alpine packages including `bash`,
`e2fsprogs`, `e2fsprogs-extra`, `xfsprogs`, `util-linux`, `coreutils`, `findutils`,
`grep`, `sed`, `gawk`, `attr`, `acl`, `quota-tools`, `fio`, `perl`, `python3` and
their recorded dependencies. Build xfstests from the locked source revision;
do not install an unpinned branch tip. Tests inspect the image manifest and
fail when a required binary, version output or APK SHA-256 is absent.

Runner tests require Bash, distinct `/dev/vda` TEST and `/dev/vdb` SCRATCH,
fresh filesystem UUIDs, empty mountpoints, no swap and a results directory on
the initramfs/host capture path rather than on either tested filesystem.

- [ ] **Step 2: Run the RED tests**

```sh
python3 -m unittest tools.tests.test_ext4_alpine_image
cargo test -p xtask ext4::xfstests -- --test-threads=1
```

Expected: tests fail because the Alpine xfstests fixture and runner do not
exist.

- [ ] **Step 3: Implement deterministic image construction**

Consume the locked APK index and local package/source artifacts. Record
`bash --version`, `mke2fs -V`, `e2fsck -V`, `xfs_info -V`, xfstests full commit,
image UUID and SHA-256. Verify `/bin/bash` is the xfstests interpreter. The
builder must be reproducible from an empty output directory and must not mutate
the source package cache.

- [ ] **Step 4: Implement isolated xfstests execution**

Create fresh TEST and SCRATCH copies, verify their hashes and device sizes,
format them according to the selected profile, export the xfstests environment
and run only the manifest's ordered IDs. Capture `check.time`, full per-case
output, dmesg/serial, mount facts and post-run `e2fsck -fn` for both images.

Classify results as `pass`, `fail`, `timeout`, `crash`, `not-run` or
`excluded-by-manifest`. Tier 1 promotion accepts only `pass` for every selected
case. Tier 2 accepts only `pass` for selected cases and validates every
exclusion against its named Tier 3 feature or separately tracked kernel
blocker. Harness/setup failure is never a skip.

- [ ] **Step 5: Verify guest and selected suites**

```sh
python3 -m unittest tools.tests.test_ext4_alpine_image
cargo test -p xtask ext4::xfstests -- --test-threads=1
cargo xtask doctor
cargo xtask ext4 xfstests --manifest tools/ext4/profiles/tier1-xfstests.json --test-image target/ext4/test.img --scratch-image target/ext4/scratch.img
cargo xtask ext4 xfstests --manifest tools/ext4/profiles/tier2-xfstests.json --test-image target/ext4/test.img --scratch-image target/ext4/scratch.img
git diff --check
```

Expected: all Tier 1 selections pass; Tier 2 has no unexplained failure,
timeout, crash or not-run result; both offline filesystem checks are clean.

- [ ] **Step 6: Commit**

```sh
git add tools/ext4/build_alpine_xfstests.py tools/tests/test_ext4_alpine_image.py tools/ext4/guest/run-xfstests.sh tools/shell-tests/alpine-ext4-xfstests.txt xtask/src/ext4/xfstests.rs xtask/src/ext4.rs xtask/src/doctor.rs
git commit -m "test(ext4): add Alpine xfstests acceptance lane"
```

### Task 6: Add stable copy, I/O, reclaim and wait attribution

**Files:**
- Modify: `schema/txobserve.toml`
- Modify: `crates/tx-subsystems/src/page_backed/mod.rs`
- Modify: `crates/tx-subsystems/src/page_backed/data_lease.rs`
- Modify: `crates/tx-subsystems/src/io_manager/block/mod.rs`
- Modify: `crates/tx-subsystems/src/io_manager/backend/graph.rs`
- Modify: `crates/tx-ext4/src/journal.rs`
- Modify: `crates/tx-ext4/src/reclaim.rs`
- Modify: `crates/tx-services/src/memory_pressure/`
- Modify: `crates/tx-reactor/src/task.rs`
- Modify: `tools/tx-observe-analyze.py`
- Modify generated by command: `crates/tx-observe/src/l0_schema/schema_catalog.rs`
- Modify generated by command: `tools/tx-observe-host-catalog.json`
- Test: `tools/tests/test_tx_observe_analyze.py`

- [ ] **Step 1: Add failing schema and accounting tests**

Declare monotonic byte/count/time counters with these required groups:

| Group | Required facts |
|---|---|
| payload | logical bytes, extra-copy bytes, aligned zero-copy bytes |
| bounce | bytes/count by alignment, segment-limit, device and metadata reasons |
| block | submitted/read/write bytes, merged bytes, queue-depth saturation |
| ext4 | extent/bitmap scan work, pager critical-section ns, lock-held-I/O count |
| journal | descriptor/metadata/commit/checkpoint bytes and retry/quarantine count |
| memory | allocation fast failures, reclaim scanned/claimed/withdrawn/freed, writeback and refault distance |
| task wait | I/O wait ns, direct-reclaim wait ns, writer-throttle ns, runnable and idle ns |

Tests take before/after snapshots, handle per-hart counters, reject decreasing
values and prove each submitted byte belongs to exactly one payload,
metadata/journal/checkpoint or explicit bounce category.

- [ ] **Step 2: Run the RED tests**

```sh
cargo xtask observe-schema check
python3 -m unittest tools.tests.test_tx_observe_analyze
cargo test -p tx-subsystems page_backed --lib -- --test-threads=1
```

Expected: required catalog entries and attribution fields are absent.

- [ ] **Step 3: Instrument authoritative owners**

Increment counters at ownership boundaries, not by sampling logs. PageBacked
counts logical payload and lease views; the only functions that actually copy
or create bounce buffers count reasoned bytes. L6 counts device submission and
merge. ext4 counts layout scan and metadata/journal/checkpoint traffic. The
memory-pressure coordinator counts candidate/claim/actual-free/refault facts.
The reactor attributes task wait states without charging runnable time as I/O
wait.

Pager critical-section timing includes service time and scanned work. Any
block-I/O submission while a pager mutation lock is held increments a hard
failure counter. Do not add tracing to the pure pager crate or put observation
callbacks into planner DTOs.

- [ ] **Step 4: Generate schema artifacts and verify accounting**

```sh
cargo xtask observe-schema codegen
cargo xtask observe-schema codegen --check
cargo xtask lint invariants observe-producer-boundary
cargo test -p tx-subsystems --lib page_backed -- --test-threads=1
cargo test -p tx-subsystems --lib io_manager -- --test-threads=1
cargo test -p tx-ext4 --lib journal -- --test-threads=1
cargo test -p tx-ext4 --lib reclaim -- --test-threads=1
python3 -m unittest tools.tests.test_tx_observe_analyze
git diff --check
```

Expected: aligned payload tests report zero extra-copy/bounce bytes; synthetic
traffic balances exactly; generated catalogs are current.

- [ ] **Step 5: Commit**

```sh
git add schema/txobserve.toml crates/tx-observe/src/l0_schema/schema_catalog.rs tools/tx-observe-host-catalog.json crates/tx-subsystems/src/page_backed crates/tx-subsystems/src/io_manager crates/tx-ext4/src/journal.rs crates/tx-ext4/src/reclaim.rs crates/tx-services/src/memory_pressure crates/tx-reactor/src/task.rs tools/tx-observe-analyze.py tools/tests/test_tx_observe_analyze.py
git commit -m "feat(observe): attribute memory and ext4 I/O costs"
```

### Task 7: Pin and run the native rustc kernel-build workload

**Files:**
- Create: `tools/ext4/workloads/rustc-kernel-build.json`
- Create: `tools/ext4/prepare_rustc_kernel_workload.py`
- Create: `tools/tests/test_ext4_rustc_workload.py`
- Create: `tools/ext4/guest/run-rustc-kernel-build.sh`
- Create: `tools/ext4/guest/run-raw-io-roofline.sh`
- Create: `xtask/src/ext4/perf.rs`
- Modify: `xtask/src/ext4.rs`

- [ ] **Step 1: Add failing workload/reproducibility tests**

The manifest requires a clean Tx source commit, `Cargo.lock` hash, vendored
dependency hash, native RV64 rustc/cargo artifact hash, complete
`rustc -vV`, target triple, linker identity, build command, environment and
image hashes. The toolchain must compile the pinned source before an image can
be marked prepared. A source/toolchain mismatch is a preparation failure, not
a performance result.

The primary command is:

```sh
cargo build --release -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf --offline --frozen
```

Prepare the native RV64 toolchain outside the timed run from the locked Rust
source/config when no verified binary artifact exists. Toolchain build time is
never included. Record its source/config and output hashes.

- [ ] **Step 2: Run the RED tests**

```sh
python3 -m unittest tools.tests.test_ext4_rustc_workload
cargo test -p xtask ext4::perf -- --test-threads=1
```

Expected: tests fail because no pinned workload/image/result schema exists.

- [ ] **Step 3: Build immutable workload and filesystem seeds**

Create a read-only WORKLOAD image containing the toolchain, source archive and
vendor tree. Create per-run TEST seeds with the source tree populated offline
so source-copy time and host cache state are not part of compilation. TEST
contains both source and Cargo target output for the representative single-FS
lane. SCRATCH is reserved for raw-roofline and harness temporary data; it is
never used to hide compiler output from the filesystem under test.

For each candidate kernel commit, preparation records the exact Git tree hash.
Dirty source trees are rejected. This intentionally pins the future repaired
implementation at measurement time instead of comparing an old pre-repair
source snapshot.

- [ ] **Step 4: Implement cold, warm and incremental lanes**

Use `-smp 1`, `-m 4096M`, no guest swap, identical raw images and QEMU cache/I/O
settings for Linux and Tx. Run:

- raw: sequential and mixed probes on a disposable SCRATCH image, reported as
  a device roofline and not as a filesystem build time;
- tmpfs: source and target on tmpfs, reported as the compiler/CPU floor;
- cold ext4: fresh boot and fresh TEST copy with empty Cargo target;
- warm ext4: immediate no-change rebuild on the same boot/image; and
- incremental ext4: apply the manifest's fixed one-file patch, then rebuild.

The bring-up gate uses three interleaved Linux/Tx pairs. Product promotion uses
five cold pairs and ten warm/incremental pairs. Never discard a completed run;
host load, QEMU exit, serial loss and counter-integrity facts remain in the
report. Reboot and clone the TEST seed for every cold pair. Use guest monotonic
wall time as primary and host wall time as a cross-check.

- [ ] **Step 5: Verify preparation and smoke performance run**

```sh
python3 -m unittest tools.tests.test_ext4_rustc_workload
cargo test -p xtask ext4::perf -- --test-threads=1
cargo xtask ext4 perf prepare --manifest tools/ext4/workloads/rustc-kernel-build.json --out target/ext4/workloads
cargo xtask ext4 perf run --manifest tools/ext4/workloads/rustc-kernel-build.json --kernel linux --lane raw
cargo xtask ext4 perf run --manifest tools/ext4/workloads/rustc-kernel-build.json --kernel tx --lane ext4 --runs 1
git diff --check
```

Expected: the same pinned source/toolchain compiles under both kernels; raw,
tmpfs and ext4 results are separate; a smoke run is not promotion evidence.

- [ ] **Step 6: Commit**

```sh
git add tools/ext4/workloads/rustc-kernel-build.json tools/ext4/prepare_rustc_kernel_workload.py tools/tests/test_ext4_rustc_workload.py tools/ext4/guest/run-rustc-kernel-build.sh tools/ext4/guest/run-raw-io-roofline.sh xtask/src/ext4/perf.rs xtask/src/ext4.rs
git commit -m "test(ext4): add native rustc kernel-build benchmark"
```

### Task 8: Generate promotion reports and enforce product gates

**Files:**
- Modify: `xtask/src/ext4/report.rs`
- Modify: `xtask/src/ext4.rs`
- Create: `tools/tests/test_ext4_acceptance_report.py`
- Create after a real run: `docs/progress/research/2026-07-25-ext4-tier1-performance-acceptance.md`
- Modify: `docs/progress/STATUS.md`
- Modify: `docs/progress/plans/2026-07-25-memory-io-ext4-repair.json`

- [ ] **Step 1: Add failing gate and report tests**

Use synthetic run sets to prove that the reporter rejects mixed hashes/hosts,
missing runs, incomplete counters, unexplained xfstests outcomes, failed fsck,
failed crash cuts, nonzero aligned payload copies/bounces and arithmetic that
includes metadata bytes in ordinary-data amplification.

The report contains per-run raw values, median paired ratios and distribution
summaries; it never reports only an average. It links every claim to the
manifest hash, result JSON, serial/observe trace and oracle output.

- [ ] **Step 2: Run the RED tests**

```sh
python3 -m unittest tools.tests.test_ext4_acceptance_report
cargo test -p xtask ext4::report -- --test-threads=1
```

Expected: tests fail because promotion rules are not executable.

- [ ] **Step 3: Encode the gates**

Correctness gates:

- every declared Tier 1 test passes and Tier 2 has no unexplained selected-case
  failure, timeout, crash or not-run;
- every deterministic cut passes its replay/data/namespace oracle and offline
  `e2fsck -fn` checks;
- Linux-to-Tx and Tx-to-Linux image exchange mounts RW and remains fsck-clean;
- reservation, lease, transaction and quarantine terminal states balance.

Ownership/accounting gates:

- `file_payload_extra_copy_bytes == 0` and
  `normal_path_bounce_bytes == 0` for aligned buffered/direct normal paths;
- ordinary-data write amplification is at most `1.3x`, computed as ordinary
  device data writes divided by logical file payload writes;
- journal, metadata freeze and checkpoint bytes are non-overlapping separately
  reported categories; and
- pager lock-held-I/O count is zero and counter accounting balances.

Performance gates against the same-run Linux median:

- bring-up cold build ratio `<= 1.50x`;
- product cold build ratio `<= 1.20x`;
- product warm and fixed incremental ratios `<= 1.15x`.

A bring-up pass authorizes profiling/tuning but is not final product
acceptance. Any timing miss must include CPU runnable/idle, task I/O wait,
direct reclaim wait, writer throttle, queue saturation, pager service/scan,
writeback, refault and device-byte attribution.

- [ ] **Step 4: Run the full acceptance ladder**

```sh
cargo xtask ext4 fault --profile tools/ext4/profiles/tier1.json --case all --cut all --iterations 1000
cargo xtask ext4 xfstests --manifest tools/ext4/profiles/tier1-xfstests.json --test-image target/ext4/test.img --scratch-image target/ext4/scratch.img
cargo xtask ext4 xfstests --manifest tools/ext4/profiles/tier2-xfstests.json --test-image target/ext4/test.img --scratch-image target/ext4/scratch.img
cargo xtask ext4 perf run --manifest tools/ext4/workloads/rustc-kernel-build.json --kernel linux --lane tmpfs --runs 5
cargo xtask ext4 perf run --manifest tools/ext4/workloads/rustc-kernel-build.json --kernel linux --lane ext4 --runs 5
cargo xtask ext4 perf run --manifest tools/ext4/workloads/rustc-kernel-build.json --kernel tx --lane tmpfs --runs 5
cargo xtask ext4 perf run --manifest tools/ext4/workloads/rustc-kernel-build.json --kernel tx --lane ext4 --runs 5
cargo xtask ext4 report --inputs target/ext4/runs --out target/ext4/report --gate product
cargo -q xtask unit
cargo xtask lint docs
cargo xtask progress validate
git diff --check
```

Expected: report exits zero only when all correctness, ownership and product
performance gates pass. Update the dated research report and progress records
with measured values, evidence paths, next step and any blocker before closing
the repair plan.

- [ ] **Step 5: Commit**

```sh
git add xtask/src/ext4/report.rs xtask/src/ext4.rs tools/tests/test_ext4_acceptance_report.py docs/progress/research/2026-07-25-ext4-tier1-performance-acceptance.md docs/progress/STATUS.md docs/progress/plans/2026-07-25-memory-io-ext4-repair.json
git commit -m "test(ext4): enforce correctness and performance acceptance"
```

## Completion rule

This subplan is complete only when Task 8's product report passes. A green host
unit suite, a boot witness, the `1.50x` bring-up milestone or a partial
xfstests selection is progress evidence, not completion. If the product timing
gate misses while correctness and accounting pass, retain the measurements and
open a separately scoped optimization plan from the dominant attributed cost;
do not weaken the architecture, fixture or oracle gate in this plan.
