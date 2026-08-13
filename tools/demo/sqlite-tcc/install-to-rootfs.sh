#!/bin/sh
# Host-side helper: install this demo directory into an Alpine rootfs.

set -eu

ROOTFS="${1:-target/rootfs/alpine-rv64-qemu}"
DEST="$ROOTFS/demo/sqlite-tcc"

if [ ! -d "$ROOTFS" ]; then
    echo "install-to-rootfs: missing rootfs: $ROOTFS" >&2
    exit 1
fi

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

rm -rf "$DEST"
mkdir -p "$DEST"
cp "$SCRIPT_DIR/README.md" "$DEST/README.md"
cp "$SCRIPT_DIR/setup-tcc.sh" "$DEST/setup-tcc.sh"
cp "$SCRIPT_DIR/run-demo.sh" "$DEST/run-demo.sh"
cp "$SCRIPT_DIR/sqlite_threads.c" "$DEST/sqlite_threads.c"
chmod +x "$DEST/setup-tcc.sh" "$DEST/run-demo.sh"

echo "install-to-rootfs: installed demo under $DEST"
