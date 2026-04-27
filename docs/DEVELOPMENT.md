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

For BusyBox initramfs generation, set `TX_BUSYBOX` to a static BusyBox binary.
If BusyBox is dynamically linked against musl, set `TX_MUSL_LIBC` to the musl
`libc.so`; the builder includes it under `/lib/libc.so` and adds musl loader
symlinks for RV64 and LA64.

The OSComp autotest suite is a submodule at `external/oscomp-autotest`.
HumanLayer's agent workflow reference is a sparse submodule at
`external/humanlayer-reference`; txKernel uses only its `.claude/` prompts as
reference material.

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
```

Build and emulator commands:

```sh
cargo xtask build --target rv64-qemu
cargo xtask build --target rv64-m1dock-mock
cargo xtask build --target la64-qemu
cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel
cargo xtask qemu --target rv64-qemu --profile busybox --dry-run
cargo xtask qemu --target rv64-m1dock-mock --profile smoke --dry-run
```

Image and submit commands:

```sh
cargo xtask image cpio --profile busybox
cargo xtask image ext4 --profile busybox --size 64M
cargo xtask image m1dock-sd --profile busybox --size 64M
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
