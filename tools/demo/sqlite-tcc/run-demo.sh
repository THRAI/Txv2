#!/bin/sh
# Guest-side one-shot TCC link runner.

set -eu

SCRIPT_DIR="${0%/*}"
SRC="${SQLITE_DEMO_SRC:-/tmp/sqlite_threads.c}"
BIN="${SQLITE_DEMO_BIN:-/tmp/sqlite_threads}"

sh "$SCRIPT_DIR/setup-tcc.sh"

cp "$SCRIPT_DIR/sqlite_threads.c" "$SRC"

set +e
tcc "$SRC" -o "$BIN"
link_status=$?
set -e

echo "tcc-link-status:$link_status"
if [ "$link_status" -ne 0 ]; then
    exit "$link_status"
fi
echo "tcc-linked-bin:$BIN"
