# Main merge network recovery validation

Date: 2026-08-01

Branch: `feature-network-refactor-recovery`

Recovery anchor: `66a846de06247e829796750e1e62ca99312d6f54`

Merged main: `5cd2f8de7f824fcb3cfc69bf42a9d9f1de617da9`

## Scope and decision

Merge current `main` into the network recovery branch with an explicit merge
commit. Preserve the pre-merge LA64 RTC/virtio-net route and RV64 Git recovery,
accept main's SMP/EBR/ext4/network changes, and repair only regressions proven by
the requested Git, netperf, and iperf witnesses. The previously proposed Phase
1–8 architecture roadmap remains deferred.

The graph had 22 main-only commits and one recovery-only commit. The three-way
preview found one textual conflict: LA64's atomic imports. Main's unconditional
`AtomicU8` import was retained because the merged SMP implementation uses it on
the target architecture. All other issues below were semantic, not textual.

## Merge-time repairs

| Seam | Symptom | Root cause | Resolution |
|---|---|---|---|
| RV64 QEMU devices | merged command placed block and net on bus 0 | main reverted the recovery branch's dedicated net bus | restore net to `virtio-mmio-bus.1` and add a combined block/net command test |
| `tx.runsh` root | Git gate mounted `vda` as root, shim writes returned `EROFS`, `/init` was absent | main's new final-image default did not preserve the legacy runsh layout | explicit `tx.root` still wins; `tx.runsh` selects tmpfs root |
| OSComp root | first netperf run mounted `vda`, corrupted the disposable image copy, then failed finals `/bin/bash` lookup | typed Oscomp/Ltp/Test modes require kernel rootfs shims but root selection ignored `BootMode` | reuse `BootMode::uses_kernel_rootfs_shims()` so compatibility modes select tmpfs root |
| LA64 deferred IRQ | HTTPS Git traffic panicked at `DeferredIrqSlot::publish` with phase `Pending` | unlike PLIC claim, reading the ExtIOI pending bitmap is non-destructive; main's kernel-interrupt windows exposed a duplicate presentation before the bottom half | record a software logical claim; acknowledge and suppress only duplicate presentations; restore ExtIOI delivery when the original owner completes |

The first LA64 repair attempt suppressed ExtIOI on the first claim. It passed the
host controller test but the guest stopped immediately after
`mount:rootfs:tmpfs:ok`. That implementation was removed. The retained software
gateway preserves the first proven route and acts only when the same source is
presented again before completion.

## Git verification

The local end-to-end gates exercise real guest Git binaries and real HTTP/HTTPS
servers, including push/pull and opt-in network IRQ accounting:

- RV64: 9/9; serial `/tmp/verifygit-wtNduS/serial.log`.
- LA64: 9/9; serial `/tmp/verifygit-T2kkDb/serial.log`;
  `claims=104`, `completions=104`, `wrong-hart=0`, `missing-device=0`.

Both guests then performed a real TLS Git clone from
`https://github.com/oscomp/xv6-riscv.git` with `--depth 1`:

- RV64: rc 0, HEAD `f5dea58cc1057f2b076cdb90b446c2c21d91171e`,
  README 2425 bytes; `/tmp/txv2-main-merge-github-shallow-clone-rv64.log`.
- LA64: the same HEAD and byte count; rc 0;
  `/tmp/txv2-main-merge-github-shallow-clone-la64.log`.

A full RV64 clone transferred 10.48 MiB without panic, allocation failure, or
protocol error, but the observed approximately 49 KiB/s external throughput
could not finish the 17.5 MiB pack inside the existing 240-second outer bound.
This is classified as a bounded external-throughput timeout; the shallow clone
retains DNS, certificate, smart-HTTP pack, index, and checkout coverage.

Using the already configured credential launcher without reading or printing
its contents, both architectures also completed a real GitHub upload loop in
the authorized `LLLPPPS/tx-push-test` repository:

- LA64 remote branch `txv2-merge-verify-la64-2020802-001`: first push, second
  clone, second push to `2e41364`, then `pull --ff-only` and readback
  `VERIFY-LA64-2`.
- RV64 remote branch `verify-rv64-20260802-1`: first push, second clone, second
  push to `8a75fa3`, then `pull --ff-only` and readback `VERIFY-RV64-2`.

RV64's second push saw two transient TLS EOFs. A subsequent `ls-remote` passed
and the next bounded retry completed. No certificate checking was disabled.
The generated RV64 credential-bearing disk copy was deleted after the test.

## netperf and iperf matrix

All runs used a separately copied, read-only-fsck-clean OSComp image set under
`target/oscomp/main-merge-data`; no source recovery image was overwritten.

| Architecture | Group | Score | Serial |
|---|---|---:|---|
| RV64 | netperf-musl | 5/5 | `target/oscomp/main-merge-20260801-netperf-musl-rv64.txt` |
| RV64 | iperf-musl | 6/6 | `target/oscomp/main-merge-20260801-netbench-rest-rv64.txt` |
| RV64 | netperf-glibc | 5/5 | same combined RV64 serial |
| RV64 | iperf-glibc | 6/6 | same combined RV64 serial |
| LA64 | netperf-musl | 5/5 | `target/oscomp/main-merge-20260801-netperf-musl-la64.txt` |
| LA64 | iperf-musl | 6/6 | `target/oscomp/main-merge-20260801-iperf-musl-la64.txt` |
| LA64 | netperf-glibc | 5/5 | `target/oscomp/main-merge-20260801-netperf-glibc-la64.txt` |
| LA64 | iperf-glibc | 6/6 | `target/oscomp/main-merge-20260801-iperf-glibc-la64.txt` |

Result: RV64 22/22, LA64 22/22, 44/44 total. Both TCP_CRR cases pass for both
libcs and architectures.

## Host gates and known baseline

- `cargo test -p xtask qemu`: 34/34.
- `cargo test -p tx-hal-loongarch64-qemu-virt`: 63/63 before the final logical
  claim regression was added; the focused claim test passes afterward.
- RV64 and LA64 debug/release builds pass.
- Focused external-connect, veth, network tick, and IRQ suites pass.

`cargo -q xtask unit` is not green on either the merge or a clean main worktree.
The matching main baseline consists of 37 tx-shims failures rooted in the first
unconnected-TCP `EAGAIN` versus `ENOTCONN` assertion, plus three tx-kernel
failures: two stale libctest launcher expectations and the existing
`run_thread` future-size ratchet. No ceiling was raised and no assertion was
weakened for this merge.

The default OSComp test-init image path is also absent in this checkout, so the
network matrix used the maintained direct compatibility entry with
`OSCOMP_TEST_INIT=0`. This is a harness/image availability issue, not a network
semantic result.

## Next and blockers

No blocker remains for the requested pre-merge network functionality or this
main merge. The two remote verification branches are intentionally left as
external witnesses; deleting them requires separate authorization. Phase 1–8,
the main-baseline unit failures, and the missing test-init artifact remain
separate work.
