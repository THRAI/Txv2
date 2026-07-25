#!/bin/sh
# Host-side helper: prepare the TCC development ext4 image used by make demo.

set -eu

STAGE="${1:-target/rootfs/alpine-tcc-dev-ext4-stage}"
IMAGE="${2:-target/images/alpine-tcc-dev-root-rv64-qemu.ext4}"
IMAGE_SIZE="${IMAGE_SIZE:-128M}"
TCC_DIR="$STAGE/usr/lib/tcc"

if [ ! -d "$STAGE" ]; then
    echo "prepare-tcc-ext4: missing staging rootfs: $STAGE" >&2
    exit 1
fi
if [ ! -x "$STAGE/usr/bin/tcc" ]; then
    echo "prepare-tcc-ext4: missing TCC binary in staging rootfs" >&2
    exit 1
fi
if [ ! -d "$TCC_DIR" ]; then
    echo "prepare-tcc-ext4: missing TCC runtime directory: $TCC_DIR" >&2
    exit 1
fi

AR="${AR:-}"
if [ -z "$AR" ]; then
    if command -v riscv64-linux-musl-ar >/dev/null 2>&1; then
        AR="riscv64-linux-musl-ar"
    elif command -v llvm-ar >/dev/null 2>&1; then
        AR="llvm-ar"
    else
        AR="ar"
    fi
fi

MKFS_EXT4="${MKFS_EXT4:-}"
if [ -z "$MKFS_EXT4" ]; then
    for candidate in \
        /opt/homebrew/opt/e2fsprogs/sbin/mkfs.ext4 \
        /opt/homebrew/Cellar/e2fsprogs/1.47.4/sbin/mkfs.ext4 \
        mkfs.ext4
    do
        if command -v "$candidate" >/dev/null 2>&1; then
            MKFS_EXT4="$candidate"
            break
        fi
    done
fi
if [ -z "$MKFS_EXT4" ]; then
    echo "prepare-tcc-ext4: mkfs.ext4 not found" >&2
    exit 1
fi

# Alpine riscv64 tcc-dev ships bcheck/runmain support objects but no
# default libtcc1.a. Putting those objects in libtcc1.a breaks normal
# pthread links by overriding musl pthread symbols and pulling unresolved
# TCC -run helpers. The default archive is only a search-path placeholder.
rm -f "$TCC_DIR/libtcc1.a" "$TCC_DIR/libtcc1.a.bad-runtime"
"$AR" rcs "$TCC_DIR/libtcc1.a"

mkdir -p "$(dirname "$IMAGE")"
if [ ! -f "$IMAGE" ]; then
    truncate -s "$IMAGE_SIZE" "$IMAGE"
fi

"$MKFS_EXT4" -F -b 4096 -I 256 -L TXTCCDEV -d "$STAGE" "$IMAGE"

echo "prepare-tcc-ext4: wrote $IMAGE from $STAGE"
