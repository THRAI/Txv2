# txKernel Development Skeleton

This repository uses `cargo xtask` as the developer command surface.

The command implementation is split by command family under `xtask/src/`; see
[`../xtask/README.md`](../xtask/README.md) for the module map. Keep wrapper
scripts thin and make them delegate to `cargo xtask`.

## Required Tools

- nightly Rust with `rustfmt`, `clippy`, `rust-src`, and `llvm-tools-preview`
- `rustup target add riscv64gc-unknown-none-elf`
- `rustup target add loongarch64-unknown-none-softfloat`
- `qemu-system-riscv64`
- `qemu-system-loongarch64`

Optional host tools for filesystem and image work:

- `cpio`
- `mkfs.ext4`
- `debugfs`
- `e2fsck`
- `mcopy`
- `zip`
- `jq` for ad hoc JSON queries; progress validation is owned by `xtask`

For BusyBox initramfs generation, either set `TX_BUSYBOX` to a static BusyBox
binary or use the in-tree vendored BusyBox for the selected target. RV64 uses
`tools/images/vendor/busybox-riscv64-musl`; LA64 uses
`tools/images/vendor/busybox-loongarch64-musl`. Build the LA64 binary either
with `docker compose run --rm busybox-la64` or, after installing a
`loongarch64-linux-musl-` cross toolchain, with
`tools/images/build-busybox-loongarch64.sh`. If BusyBox is dynamically linked
against musl, set `TX_MUSL_LIBC` to the musl `libc.so`; the builder includes it
under `/lib/libc.so` and adds musl loader symlinks for RV64 and LA64.

## Docker workflow

Txv2 ships a `docker-compose.yml` with:

- `oscomp`: general build/test/qemu container (`cargo xtask ...`)
- `busybox-la64`: dedicated LA64 BusyBox builder

Common entry points (from workspace root):

```sh
make docker-build
make docker-shell
make docker-ci
make docker-build-la64
make docker-busybox-la64
make docker-image-cpio-la64
make docker-qemu-la64-busybox
```

OSComp flow in container:

```sh
make docker-oscomp-doctor
make docker-oscomp-prepare
make docker-oscomp-submit
make docker-oscomp-run
```

If a previous root-run container left root-owned files in your workspace, run
`busybox-la64` as your uid/gid:

```sh
TX_DOCKER_UID=$(id -u) TX_DOCKER_GID=$(id -g) docker compose run --rm busybox-la64
```

The OSComp autotest suite is a submodule at `external/oscomp-autotest`.
HumanLayer's agent workflow reference is a sparse submodule at
`external/humanlayer-reference`; txKernel uses only its `.claude/` prompts as
reference material.
Musl libc is available as a reference-only submodule at `external/musl`; use
`arch/riscv64/` plus `arch/generic/bits/` and `include/sys/` when checking
userspace ABI layouts such as SysV IPC headers.

Initialize submodules with:

```sh
git submodule update --init --recursive
git -C external/humanlayer-reference sparse-checkout init --no-cone
git -C external/humanlayer-reference sparse-checkout set '/.claude/*'
```

## Command Families

```sh
cargo xtask doctor
cargo xtask ci
cargo xtask check
cargo -q xtask unit
```

Build and emulator commands:

```sh
cargo xtask build --target rv64-qemu
cargo xtask build --target rv64-m1dock-mock
cargo xtask build --target la64-qemu
cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel
cargo xtask qemu --target rv64-qemu --profile busybox --dry-run
cargo xtask qemu --target la64-qemu --profile busybox --dry-run
cargo xtask qemu --target rv64-m1dock-mock --profile smoke --dry-run
```

Image and submit commands:

```sh
docker compose run --rm busybox-la64
cargo xtask image cpio --profile busybox --target la64-qemu
cargo xtask image ext4 --profile busybox --target la64-qemu --size 64M
cargo xtask image m1dock-sd --profile busybox --target rv64-m1dock-mock --size 64M
cargo xtask submit k210
```

Agent progress and lint commands:

```sh
cargo xtask progress validate
cargo xtask progress list all --json
cargo xtask progress new plan --id YYYY-MM-DD-short-title --title "Short title" --scope path/prefix
cargo xtask progress claim plan --id YYYY-MM-DD-short-title --owner agent-name --scope path/prefix
cargo xtask progress close plan --id YYYY-MM-DD-short-title --status complete
cargo xtask lint arch
cargo xtask lint docs
```

The current milestone is compile-first, not boot-first. QEMU command generation
and BusyBox image wiring are present so the next boot-stub milestone has a
stable tool contract to build on.

`cargo -q xtask unit` is the fast local check: builds tx-shims, tx-kernel,
tx-ext4, and tx-scripts, then runs each `--lib` test suite with
`--test-threads=1`. Output is one line per step on pass; on failure it shows
only the failing test name, panic message, and failure list. The `-q` flag
suppresses cargo's own build headers.

`cargo xtask ci` is the fast CI-facing reporter. It prints concise pass/skip
lines and expands failed checks with command, status, captured output, and
`txdoc:` references into [`design/00_meta-framework/CI_REPORTING_v1.md`](design/00_meta-framework/CI_REPORTING_v1.md).
`cargo xtask ci-slow` is the QEMU lane: it builds RV64 QEMU and requires the
serial sentinel `txkernel:qemu-riscv64-virt:boot:ok` to appear before timeout.
Serial logs are written under `target/qemu-*.serial.log`.

## OSComp Autotest

```sh
cargo xtask oscomp doctor
cargo xtask oscomp prepare --data target/oscomp/testdata
cargo xtask oscomp submit --submit target/oscomp/submit
cargo xtask oscomp run --data target/oscomp/testdata --submit target/oscomp/submit --dry-run
cargo xtask oscomp qemu --target rv64-qemu --data target/oscomp/testdata --dry-run
```

`oscomp prepare` copies the judge scripts and creates
`target/oscomp/cg/kernel.zip`. It does not download the large SD card images;
place `sdcard-rv.img.gz` and `sdcard-la.img.gz` in the data directory shown by
the command.

The image-builder scripts are thin wrappers over xtask:

```sh
tools/images/build-cpio.sh
tools/images/build-ext4.sh --size 64M
```

## Debug tooling

txKernel ships two paired debug-instrumentation surfaces. Both are
designed for ad-hoc triage on real QEMU runs without forcing a
production-overhead trace.

### `cargo xtask fault-decode` — single-trap analysis

Decodes a kernel-mode trap dump into a resolved ELF symbol +
runtime classification. Reads `txkernel:<board>:trap` records (the
panic-path dump emitted by `tx_rv64_qemu_trap_panic` and similar)
or accepts raw `--scause`/`--sepc`/`--stval` triples.

```sh
# parse a panic dump from a serial log
cargo xtask fault-decode --target rv64-qemu --serial target/qemu-rv64-qemu-busybox.serial.log

# decode a single address
cargo xtask fault-decode --target rv64-qemu --addr 0xffffffff802d32a0
```

Handles low-linked / high-VMA layouts, demangles Rust symbols, and
classifies kernel direct-map vs ELF-text pointers.

### `cargo xtask trap-trace` + `--features trap-trace`

Streams every user-mode trap and every userspace re-entry as a
single-line `txdbg:` record on the serial console. Off by default
(zero overhead in production builds); enabled either at cargo
build time:

```sh
cargo build -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf \
  --features trap-trace
```

or via the test lane's flag:

```sh
cargo xtask test busybox-boot --target rv64-qemu --trap-trace
```

Wire format (canonical, see
`boards/tx-hal-riscv64-qemu-virt/src/debug_trace.rs`):

```text
txdbg:trap n=0x... kind=SY pc=0x... a7=0x... a0=0x... a1=0x... a2=0x...
txdbg:trap n=0x... kind=iPF|lPF|sPF|? pc=0x... stval=0x... ra=0x... a0=0x... a1=0x...
txdbg:ent  n=0x... pc=0x... a0=0x... sp=0x...
```

Each record is one line, prefixed `txdbg:` for grep, with
`key=0xHEX` pairs. The trap counter is monotonic; `txdbg:trap n=K`
is paired with the next `txdbg:ent n=K+1` to show the syscall
return value (or fault retry pc) the kernel handed back to
userspace.

The companion parser turns the stream into a paired summary:

```sh
# full timeline (faults + syscalls)
cargo xtask trap-trace --serial target/qemu-rv64-qemu-busybox-smp1.serial.log

# syscall-only filter, with NR mnemonics + errno decoding on returns
cargo xtask trap-trace --serial target/qemu-rv64-qemu-busybox-smp1.serial.log --syscalls

# pass-through grep `txdbg:` lines
cargo xtask trap-trace --serial target/qemu-rv64-qemu-busybox-smp1.serial.log --raw
```

For multi-syscall triage, use `-smp 1` so the records aren't
interleaved across harts:

```sh
qemu-system-riscv64 -machine virt -m 256M -smp 1 -display none -monitor none \
  -serial file:target/qemu.serial.log -no-reboot \
  -kernel target/riscv64gc-unknown-none-elf/debug/tx-kernel-riscv64-qemu-virt \
  -bios default -no-shutdown \
  -initrd target/images/busybox-initramfs-rv64-qemu.cpio \
  -append 'tx.profile=busybox console=ttyS0'
cargo xtask trap-trace --serial target/qemu.serial.log --syscalls
```

Adding new trace records:

1. Bump or add a record-kind tag in `debug_trace.rs` (use the
   existing `record_trap` / `record_entry` shape).
2. Update the parser in `xtask/src/trap_trace.rs` to recognise the
   new kind.
3. The record format is intentionally grep-stable; do not break
   existing kinds without bumping a version sentinel.

## M1 Dock Mock Target

`rv64-m1dock-mock` is a QEMU `virt` runner for a Sipeed M1 Dock-like board
profile. It does not claim QEMU emulates the K210. The static platform crate
declares an SPI0 CS0 SD-card fact, while QEMU supplies an image-backed block
device at `target/images/m1dock-sd.img` so the early kernel path can be tested
before real SPI MMIO or hardware-in-loop is available.

The driver scaffold lives in `tx-drivers::sd_spi` and models the SD-over-SPI
command sequence (`CMD0`, `CMD8`, `CMD55`, `ACMD41`, `CMD16`, `CMD17`,
`CMD24`) with unit-testable SPI transport traits.

## Clean K210 Submit Tree

The main workspace is intentionally larger than a contest submission. Generate a
small standalone submit tree with:

```sh
cargo xtask submit k210
```

The output defaults to `target/submit/k210` and contains only the Rust crates,
M1 Dock board crates, `Cargo.toml`, `Cargo.lock`, `.cargo/config.toml`,
`rust-toolchain.toml`, and a root `Makefile`. Inside that directory:

```sh
make all
```

builds the selected K210 package and objcopies it to `k210.bin`, matching the
submission contract. The generated tree is disposable; regenerate it rather
than editing it by hand.
