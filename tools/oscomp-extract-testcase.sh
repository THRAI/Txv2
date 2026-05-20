#!/usr/bin/env bash
# Extract the OSComp sdcard images into a Chronix-like visible testcase tree.
#
# Usage:
#   tools/oscomp-extract-testcase.sh [testdata-dir] [output-dir]
#
# Output layout:
#   output-dir/
#     riscv/{musl,glibc,...}
#     loongarch/{musl,glibc,...}

set -euo pipefail

DATA_DIR="${1:-target/oscomp/testdata}"
OUT_DIR="${2:-target/oscomp/testcase}"

RV_IMAGE="$DATA_DIR/sdcard-rv.img"
LA_IMAGE="$DATA_DIR/sdcard-la.img"

log() {
  printf '[oscomp-extract-testcase] %s\n' "$*" >&2
}

need_file() {
  if [ ! -f "$1" ]; then
    log "missing $1"
    log "run: make docker-oscomp-prepare"
    exit 1
  fi
}

if ! command -v 7z >/dev/null 2>&1; then
  log "7z is required to extract ext4 images"
  exit 1
fi

need_file "$RV_IMAGE"
need_file "$LA_IMAGE"

extract_one() {
  local arch="$1"
  local image="$2"
  local dest="$OUT_DIR/$arch"
  local status

  log "extracting $image -> $dest"
  rm -rf "$dest"
  mkdir -p "$dest"

  set +e
  7z x -y "-o$dest" "$image" '-x![SYS]' '-x!lost+found'
  status=$?
  set -e

  # 7z reports official OSComp ext4 metadata checksum warnings as exit code 2,
  # while still extracting the regular file tree correctly.
  if [ "$status" -ne 0 ] && [ "$status" -ne 1 ] && [ "$status" -ne 2 ]; then
    log "7z failed for $image with status $status"
    exit "$status"
  fi

  rm -rf "$dest/[SYS]" "$dest/lost+found"
  find "$dest" -type f -name '*.sh' -exec chmod +x {} +
  log "done $arch"
}

rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"

extract_one riscv "$RV_IMAGE"
extract_one loongarch "$LA_IMAGE"

log "testcase tree ready at $OUT_DIR"
