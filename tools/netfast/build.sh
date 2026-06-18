#!/bin/sh
# Build tx-netfast and vendor the rv64 blob for the kernel embed.
# Usage: tools/netfast/build.sh [host]
#   (no arg)  build riscv64 blob into tools/images/vendor/tx-netfast-riscv64
#   host      build a host (x86_64) binary into tools/netfast/tx-netfast-host
#             for applet unit testing
set -eu
cd "$(dirname "$0")"

CFLAGS="-static -nostdlib -nostartfiles -ffreestanding -fno-builtin \
 -fno-pie -no-pie \
 -fno-stack-protector -fno-asynchronous-unwind-tables -Os -Wall -Wextra"

if [ "${1:-}" = host ]; then
    gcc $CFLAGS -o tx-netfast-host netfast.c
    exit 0
fi

CC="${CC:-riscv64-linux-gnu-gcc}"
OUT=../images/vendor/tx-netfast-riscv64
$CC $CFLAGS -o "$OUT" netfast.c
"${CROSS_STRIP:-riscv64-linux-gnu-strip}" "$OUT" 2>/dev/null || true
sha256sum "$OUT" > "$OUT.sha256"
ls -l "$OUT"
