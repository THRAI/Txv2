# Ext4 Rustc Materialization Path

Date: 2026-08-09

The reconciliation worktree now has a bounded materializer for the frozen
native-RV64 rustc workload. It creates separate TEST, SCRATCH, and WORKLOAD
ext4 images, writes source/vendor and Cargo offline configuration only to
TEST, and copies the toolchain only into WORKLOAD. The resolver shim installer
accepts that copied toolchain path and does not mutate a generic Alpine rootfs.

`qemu` and `shell-test` accept `--memory-mib`; defaults are unchanged. The
rustc runner uses named role flags, requests 1 vCPU and 4096 MiB, mounts vda
read-write and vdc read-only, requires `rustc -vV`, then runs the frozen
offline kernel build.

Focused xtask/Python checks and role dry-run rendering passed. The Homebrew
e2fsprogs path also produced all three role images from a temporary clean
fixture source; each passed `e2fsck -fn`. `debugfs` showed source/vendor only
on TEST and `libtx-rustc-resolv-preload.so` only under WORKLOAD/toolchain.
The fixture still used fake compiler binaries, so no RV64 guest success claim
is made. The measured clean12 fixture was later reused with the real RV64
toolchain. The loader-only witness passed `rustc -vV` and `cargo -V` with
TEST=`vda`, SCRATCH=`vdb`, and read-only WORKLOAD=`vdc`. Cargo metadata also
completed with `--offline --manifest-path /mnt/ext4-test/source/Cargo.toml`.
The production runner required three portability fixes: avoid `cd` into the
ext4 source tree (use an absolute manifest path), export loader/toolchain
variables for the TEST-side rustc wrapper, and use `set -eu` for Alpine
BusyBox `sh`. The sequential shell-test harness now polls `try_wait()` during
`wait`/`expect`, joins output readers before serial-log capture, and therefore
reports QEMU early exits instead of waiting through a multi-hour marker timeout.

A full default-kernel frozen kernel build was then bounded at 30 minutes. It
proved the real `rustc -vV` phase and remained CPU-bound in Cargo, but produced
no `TX_GUEST_RUST_BUILD status=0` marker before the 1,800,000 ms deadline.
The run is evidence of a remaining guest build-progress/performance blocker,
not an acceptance receipt. The trap-trace metadata run is diagnostic only and
must not be used as a production performance measurement. M1/M2 and M3 remain
open; Tier 1 crash/xfstests acceptance is unaffected.

The guest runner source now emits `TX_GUEST_RUST_STAGE` markers before and
after the `rustc -vV` probe and after the Cargo build, and enables Cargo `-vv`
output for the next bounded probe. The follow-up live probe reused the already
materialized clean12 TEST image, so it reached the same `rustc -vV` output but
cannot validate the new markers; this leaves Cargo startup or the first
compiler invocation as the next bounded investigation, without changing the
full-build acceptance requirement.
