# opensbi-silent

`fw_dynamic.bin` is OpenSBI v1.8.1 built with `FW_OPTIONS=0x1`
(`SBI_SCRATCH_NO_BOOT_PRINTS`), which suppresses the boot banner and all
M-mode startup prints. Used by `cargo xtask qemu` in place of the
QEMU-bundled `opensbi-riscv64-generic-fw_dynamic.bin`.

## Why

QEMU's bundled OpenSBI hardcodes `.options = 0` in `hw/riscv/boot.c` with no
CLI flag to change it, so the banner cannot be silenced at runtime.

## Rebuild

Requires clang + ld.lld (LLVM). On macOS with Homebrew:

```sh
brew install llvm@20 lld@20

LLVM_BIN=$(brew --prefix llvm@20)/bin
LLD_BIN=$(brew --prefix lld@20)/bin
mkdir -p /tmp/opensbi-llvm-bin
ln -sf "$LLVM_BIN/clang"          /tmp/opensbi-llvm-bin/clang
ln -sf "$LLD_BIN/lld"             /tmp/opensbi-llvm-bin/ld.lld  # wrapper script: exec lld -flavor ld.lld
chmod +x /tmp/opensbi-llvm-bin/ld.lld
for t in ar objcopy objdump nm strip; do
  ln -sf "$LLVM_BIN/llvm-$t" /tmp/opensbi-llvm-bin/llvm-$t
done

curl -sL https://github.com/riscv-software-src/opensbi/archive/refs/tags/v1.8.1.tar.gz \
  | tar xz --strip-components=1 -C /tmp/opensbi-src

PATH="/tmp/opensbi-llvm-bin:$PATH" \
make -C /tmp/opensbi-src PLATFORM=generic FW_OPTIONS=0x1 \
     LLVM="/tmp/opensbi-llvm-bin/" -j$(nproc)

cp /tmp/opensbi-src/build/platform/generic/firmware/fw_dynamic.bin \
   external/opensbi-silent/fw_dynamic.bin
```
