# Loongson 2K1000 AHCI write experiment protocol

Date: 2026-08-11  
Branch: `feature/portable-net-vf2-dwmac`  
Implementation baseline: `42cd4396f97d466ca70eb006c91ddd543d153e99`  
Operational plan: [`../plans/2026-08-11-la2k1000-ahci-write.json`](../plans/2026-08-11-la2k1000-ahci-write.json)

## 1. Outcome and fixed boundary

The target outcome is a bounded, polling, single-slot Loongson 2K1000 AHCI
write path that can safely back the existing raw ext4 root filesystem:

1. one 4 KiB kernel block request becomes one eight-sector ATA
   `WRITE DMA EXT` command through the already-owned bounce buffer;
2. an ext4 barrier reaches one ATA `FLUSH CACHE EXT` command and returns only
   after the device reports completion or failure;
3. a first runtime transport failure leaves one compact diagnostic witness
   without printing every command;
4. host and physical-target gates pass before a reset is requested;
5. no real medium is written until the exact medium and filesystem-level
   probe are approved.

The following facts are already closed and are not experiments in this plan:

- the LA QEMU Pmap/IPI/TLB repair is preserved;
- the 2K1000 CPU-pin failure was a 64 KiB runtime-stack overwrite and is closed;
- file-I/O/service fairness and full clone/post-clone stability are closed;
- AHCI MMIO, IRQ, DMA-domain, disk capacity, raw-ext4-at-LBA0, and cache facts
  were already established;
- the local Alpine mother image is structurally readable and is not a target
  of this experiment;
- the observed `Invalid argument` write failure is the bridge-visible result
  of the AHCI driver returning `EROFS`, not evidence of a cross-platform ext4
  algorithm failure.

## 2. Canonical Gate

| Proposed item | Authority | Gate result |
|---|---|---|
| Reuse the existing statically bound `AhciBlock<P>` and typed device DMA domain | `DEVRES-3`, `DEVRES-6`; current AHCI binder and workspace | admitted; no runtime HAL manager or board-name branch |
| Keep the pinned 8 KiB workspace and 4 KiB 32-bit bounce region | `HAL-DMAIF-1`; `PAGE_SUBSTRATE_v1` §5.3; current read path | admitted; no direct caller-frame DMA in this phase |
| Add data-out direction to the current command engine | Intel AHCI 1.3.1 command-header W contract; current data-in engine | admitted with host red test for W and PRDT shape |
| Implement `WRITE DMA EXT` (`0x35`) for LBA48 devices | ATA8-ACS §7.62; existing LBA48 IDENTIFY/READ path | admitted; non-LBA48 operation is rejected rather than guessed |
| Implement `FLUSH CACHE EXT` (`0xEA`) as a non-data command | ATA8-ACS §7.15; `TX_EXT4_PLAN_v1_2.md` BlockDevice barrier contract | required, not optional |
| Override `BlockDeviceImage::barrier()` and delegate to `BlockDeviceOps::barrier()` | existing `BlockImage` method; `TX_EXT4_PLAN_v1_2.md` journal ordering | required; default success is not acceptable for a disk backend |
| Preserve `EROFS` and `EIO` across the ext4 bridge | existing `Errno` block contract; explicit user requirement for diagnosable experiments | admitted only through narrow format-error variants and exhaustive mapping tests |
| Add one first-failure AHCI register snapshot | explicit handoff requirement for low-disturbance observation; existing failure-only DWMAC pattern | admitted; no success-path/per-command logging |
| Run writes on an approved filesystem object, never a guessed raw LBA | existing raw ext4 layout plus data-protection requirement | mandatory safety gate |

No new VFS entity, mount type, HAL manager, block-device trait method, DMA
domain, driver registry, or architecture-wide policy is introduced.

### RV implementation comparison

The RV64 VF2 MMC driver confirms the filesystem-facing unit conversion, but
not the persistence contract. Its `write_blocks_bootstrap()` walks each 4 KiB
`Frame`, divides it into eight 512-byte chunks, issues CMD24 once per chunk,
and stops on the first failed command. The AHCI implementation keeps the same
Frame-to-eight-sector contract and first-error stop rule, but represents the
eight contiguous sectors with one `WRITE DMA EXT` command and the existing
single PRDT bounce buffer.

The RV driver cannot be copied as a durability implementation: both its
runtime and bootstrap `barrier()` methods return success without sending a
device command. That is sufficient to show that ext4 mutation is not
intrinsically broken on RV, but it does not prove journal ordering across a
power loss. The 2K1000 acceptance therefore requires a real
`FLUSH CACHE EXT` completion and a reset/persistence witness instead of
adopting the RV no-op.

## 3. Hypotheses

| ID | Hypothesis | Observable proof | Falsifier / stop condition |
|---|---|---|---|
| H1 | The ext4 write failure is caused by AHCI returning `EROFS` before command issue | host bridge test sees `EROFS`; implementation changes only transport and the same ext4 mutation becomes writable | write still fails before the AHCI write counter/command path |
| H2 | The current read workspace is sufficient for one-page writes | a 4 KiB source pattern is copied to the same bounce address, described by one PRDT entry, and PRDBC reaches 4096 | address/length constraint failure or partial PRDBC |
| H3 | LA64 non-coherent correctness is obtained through the existing `DmaIf` seam | write bounce uses `ToDevice`, command table uses `ToDevice`, command header is synchronized for HBA updates, and publication uses `dbar` | a code path consults global coherency, truncates a DMA address, or omits publication |
| H4 | A non-data flush is required for ext4 journal ordering | `BlockImage::barrier` reaches AHCI, creates PRDTL=0/W=0, and waits for `0xEA` completion | bridge uses the default no-op barrier or driver returns before device completion |
| H5 | Failure-only observation is enough for the first hardware pass | one failure reports operation/LBA/error and PxIS/PxTFD/PxSERR/PxCI; successful reads/writes print nothing | serial traffic appears per command or a transport failure has no identifying state |
| H6 | A filesystem-level, barriered write survives reset without new ext4 damage | deterministic hash survives power cycle and offline `e2fsck -f -n` is clean | hash mismatch, AHCI error, journal replay anomaly, or new fsck finding |

## 4. Required command semantics

### 4.1 Data-in control

The existing `READ DMA EXT` path is the unchanged control:

- command FIS `0x25`, LBA48, sector count 8;
- Command Header CFL=5, W=0, PRDTL=1;
- one PRDT entry with DBC=4095;
- bounce sync `FromDevice`, PxCI issue, completion/error poll, CPU sync, and
  PRDBC=4096 before Frame copy.

Every refactor must keep the current read FIS and tests byte-compatible.

### 4.2 Data-out command

For each source `Frame`:

1. reject an uninitialized controller, empty range overflow, capacity overflow,
   non-LBA48 address, invalid Frame mapping, or kernel-image overlap;
2. copy exactly 4096 bytes from the Frame direct-map address into the private
   bounce region;
3. build a register H2D FIS with command `0x35`, the 48-bit LBA, device LBA bit,
   and sector count 8;
4. build Command Header CFL=5, W=1, PRDTL=1, PRDBC=0;
5. point one PRDT entry at the bounce DMA address with DBC=4095;
6. synchronize command header as bidirectional, command table as `ToDevice`,
   received FIS as `FromDevice`, and bounce as `ToDevice`;
7. execute `publish_to_device()` before writing slot 0 to PxCI;
8. stop on PxIS error bits, PxTFD.ERR, wall-clock timeout, or stalled-clock
   escape; after completion synchronize HBA-written state for the CPU;
9. require PRDBC=4096 and return `EIO` for any transport failure.

The first implementation deliberately executes one Frame per command. It does
not batch multiple pages, use NCQ, or DMA caller frames directly.

### 4.3 Non-data flush

`FLUSH CACHE EXT` uses the same free-slot, TFD-ready, status-clear,
publication, PxCI, completion, and error machinery, with these differences:

- command FIS `0xEA`;
- CFL=5, W=0, PRDTL=0, PRDBC=0;
- no PRDT entry and no bounce synchronization;
- no PRDBC transfer-length check;
- a flush-specific wall-clock deadline of at least 60 seconds;
- the stalled-clock iteration escape counts only consecutive failure of the
  time source to advance, rather than capping all normal polls.

ATA permits a cache flush to take longer than 30 seconds. A total ten-million
iteration limit is therefore not a valid substitute for a wall-clock flush
deadline.

## 5. Host experiment sequence

### E0 — baseline and plan validation

Commands:

```sh
git status --short --branch
cargo xtask progress validate
cargo xtask lint docs
git diff --check
```

Pass: the new JSON parses and its links are valid. Existing unrelated failures
are recorded exactly. Do not edit old ratchets, TLB code, or hardware facts to
make this phase green.

### E1 — command-layout red tests

Add pure host tests before constants/helpers exist:

- write FIS contains `0x35`, all six LBA bytes, LBA device bit, and count 8;
- data-out Command Header has W=1 and PRDTL=1;
- read Command Header remains W=0;
- PRDT DBC is `transfer_len - 1` and exactly 4095 for one page;
- flush FIS contains `0xEA`, W=0, PRDTL=0, and no PRDT requirement;
- range validation rejects arithmetic overflow, media overflow, and addresses
  outside 48 bits.

Record the expected failures, then implement until only these tests turn green.

### E2 — command-engine implementation

Refactor the current data-in-only helpers around a closed internal command
shape such as `DataIn`, `DataOut`, and `NonData`. This is an AHCI-private enum,
not a new subsystem catalog. Verify header flags, PRDT presence, DMA direction,
timeout class, and PRDBC expectation are derived from that shape.

Pass:

```sh
cargo test -p tx-drivers --lib ahci::tests::
cargo test -p tx-drivers --lib
```

Stop if any pre-existing read/identify/poll/detach test regresses.

### E3 — ext4 barrier and error bridge

First add a recording block device with a barrier counter and programmable
`Done`, `Continue`, `Yield`, `EROFS`, and `EIO` outcomes. Then require:

- `BlockDeviceImage::barrier()` calls the underlying barrier exactly once;
- `Continue` and `Yield` become `WouldBlock`;
- `EROFS` does not become `Truncated/EINVAL`;
- `EIO` does not become an on-disk corruption classification;
- journal tests still observe descriptor/payload → barrier → commit → barrier.

The smallest accepted error extension is format-layer `ReadOnly` and `Io`; it
must remain independent of kernel `Errno` types and map back to `EROFS`/`EIO`
only in the kernel-facing tx-ext4 layer.

### E4 — low-disturbance observation

On the first transport failure per boot, record:

```text
txkernel:ahci:io-error:op=<read|write|flush>:lba=<hex>:error=<label>:
  is=<hex>:tfd=<hex>:serr=<hex>:ci=<hex>
```

The physical line may be compacted, but it must preserve all fields. Maintain
a relaxed cumulative failure count and a first-failure latch. Do not print
successful commands, Frame addresses, bounce contents, every retry, or every
writeback. Input validation errors such as an out-of-range caller request are
not controller failures and do not emit this record.

### E5 — host/build admission

Run in this order after code edits:

```sh
cargo -q xtask unit
cargo test -p tx-drivers --lib ahci::tests::
cargo test -p tx-drivers --lib
cargo test -p tx-fs --lib tx_ext4_bridge::tests::
cargo test -p tx-ext4-format --lib
cargo test -p tx-ext4 --lib
cargo fmt --all -- --check
cargo xtask full-build --target la64-2k1000 --skip-doctor --no-image
git diff --check
```

Changed-path focused tests and the physical-target build are hard gates.
Repository-wide failures already present at the baseline are not silently
fixed, waived, or attributed to this work.

## 6. Real-board experiment sequence

No real-board phase begins until E5 passes. Codex operates the serial bridge,
captures logs, sends guest commands, and diagnoses failures. The user performs
only requested resets and approves the exact writable medium.

### B0 — read-only control

Boot with `tx.root=sda ro` and the existing Alpine root. Verify capacity,
superblock, mount, known file hashes, both CPUs, network, ordinary shell, and
absence of AHCI/pin/stack/panic reports. Do not remount writable and do not
create a probe file.

Pass means the new command refactor did not regress the existing read path.
Only after this control passes may a writable experiment be proposed.

### B1 — disposable filesystem write

Preferred medium: a byte-for-byte clone or spare SATA device explicitly
identified by the user. Never use an assumed unused LBA, because the device is
a raw ext4 filesystem and a guessed block may be live metadata.

On the approved filesystem, use one dedicated directory and deterministic
payload:

1. create `/root/tx-ahci-write-probe/phase1.tmp` exclusively;
2. write exactly 4096 deterministic bytes and record the hash;
3. `fsync` the file;
4. rename it to `phase1.durable` and `fsync` the containing directory;
5. read and verify the hash;
6. call `sync` only as a supplementary whole-system drain, not as a substitute
   for the file/directory fsync witnesses;
7. reset/power-cycle and verify the durable name/hash;
8. delete the file, fsync the directory, power-cycle, and verify absence;
9. boot an environment in which the filesystem is unmounted and run
   `e2fsck -f -n`.

Stop immediately on the first AHCI record, `EIO`, timeout, partial hash,
unexpected journal recovery, filesystem remount-ro, panic, or new fsck report.
Preserve the complete serial boundary and do not retry writes after a transport
error in the same boot.

If no disposable medium or offline fsck environment exists, stop after B0 and
ask the user for the exact approval/alternative. This plan does not silently
promote the competition disk to disposable status.

### B2 — onboard filesystem acceptance

Requires explicit user approval after B1 is complete. Repeat the same bounded
directory protocol on the onboard filesystem before changing configuration.
Then, in order:

1. persist the intended resolver file and read it after reset;
2. persist CA/TLS material only through ordinary files and verify hashes;
3. restore UTC with `date -u` after each reset (this is runtime state, not a
   disk-persistence witness);
4. keep TLS certificate verification enabled;
5. run local Git init/add/commit/fsync-style workload;
6. run one bounded HTTPS clone and post-clone file/status/log operations;
7. run the previously closed sustained TLS/fairness witness;
8. finish with an offline filesystem check when the test environment permits.

`HOME` remains `/root`; no `/musl` mount or alternate dynamic-linker layout is
introduced.

## 7. Stop, rollback, and recovery

- Host regression: stop at the failing phase, keep the red test and exact
  output, and do not deploy.
- Read-only board regression: reboot the previously committed image; no medium
  recovery is needed because no write was authorized.
- First write error: stop all further I/O in that boot, preserve the first
  failure record, reset only after analysis, and inspect the filesystem
  offline before reuse.
- Filesystem inconsistency: do not run repairing fsck automatically. Preserve
  the medium and ask the user before any repair or restore.
- Code rollback: revert only the scoped AHCI/bridge commit. Do not revert the
  LA QEMU TLB, CPU-pin/stack, or fairness repairs.
- Image rollback: use the prior verified TFTP kernel/initrd artifacts. Never
  overwrite the Alpine mother image as a rollback mechanism.

## 8. Evidence ledger

For every completed phase, record:

- exact Git commit and dirty-tree state;
- exact command and result counts;
- first failing assertion or serial marker, not only the final exit status;
- kernel/uImage/initrd hashes for anything deployed;
- boot arguments and whether the filesystem was `ro` or `rw`;
- writable medium identity and the user's approval boundary;
- probe path, payload length/hash, fsync/rename/delete results;
- AHCI first-failure record or an explicit statement that none appeared;
- offline fsck command and output status;
- next stop/go decision.

The plan remains active after host implementation. It is complete only when
the authorized hardware phase being claimed has evidence; code compilation
alone does not claim competition-disk durability.

## 9. B0 read-only result

The first real-board control passed on 2026-08-11 without authorizing a media
write. Commit `73819f58` produced a kernel uImage with a 7,164,144-byte payload
and deployed SHA-256
`1bb5134d56bf463ca11638b54ae257fc86ce6c515d14c5a37c706312677d43cb`.
The previous kernel remains recoverable as
`txv2-la2k1000.pre-73819f58.uimage`; the validated Alpine initrd was not
replaced.

U-Boot verified both legacy image CRCs. The exact root boundary was
`tx.root=sda ro tx.mount.sdcard=0 init=/bin/sh`; txKernel identified
62,533,296 512-byte sectors, mounted `/dev/sda / ext4 ro`, entered the Alpine
3.21 shell, and read `/bin/busybox` as SHA-256
`ee648c5338186ee244098b09f0e691dcb49ca7bd75028ec24629cc817789ed60`.
Both runtime-stack waterlines remained at the prior safe values, the GMAC
interface remained available, and one host ping passed. The complete 246-line
log is `msp/serial/2026-08-11-ahci-write-ro.log`; it contains no
`txkernel:ahci:io-error`, panic, CPU-pin, guard, or cmdline/state corruption
marker.

B1 remains blocked on media authorization. The running board stays read-only.
No remount, create, write, rename, fsync, raw-LBA operation, or filesystem
repair command was issued.
