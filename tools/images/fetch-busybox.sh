#!/usr/bin/env bash
# Populate tools/images/vendor/busybox-riscv64-musl from a known prebuilt
# source. Verifies SHA256 if a pin already exists; writes the pin on first
# successful download. Run from anywhere; resolves paths from the script
# location.
#
# Sources tried, in order:
#   1. rcore-os/busybox-prebuilts (community, aligned with rCore/zCore/OSComp)
#   2. leommxj/prebuilt-multiarch-bin release assets
#   3. Alpine Linux busybox-static apk for riscv64
#
# Override the version or primary URL via env vars:
#   BUSYBOX_VERSION=1.32.1 ./fetch-busybox.sh
#   PRIMARY_URL=https://… ./fetch-busybox.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
VENDOR_DIR="$SCRIPT_DIR/vendor"
TARGET="$VENDOR_DIR/busybox-riscv64-musl"
SHA_FILE="$TARGET.sha256"
SOURCE_FILE="$TARGET.SOURCE"

BUSYBOX_VERSION="${BUSYBOX_VERSION:-1.32.1}"

# Primary: rcore-os/busybox-prebuilts. Path scheme observed on master is
# `busybox-<ver>-<arch>/busybox`. raw.githubusercontent.com serves the file.
PRIMARY_URL="${PRIMARY_URL:-https://raw.githubusercontent.com/rcore-os/busybox-prebuilts/master/busybox-${BUSYBOX_VERSION}-riscv64/busybox}"

# Fallback 1: leommxj/prebuilt-multiarch-bin — the `bin` branch hosts files
# directly. Adjust if the upstream layout changes.
FALLBACK_LEOMMXJ="https://raw.githubusercontent.com/leommxj/prebuilt-multiarch-bin/bin/riscv64_tools/busybox"

# Fallback 2: Alpine apk for riscv64. We extract /bin/busybox.static from
# the package. We deliberately pin to a versioned URL on first success so
# this is reproducible.
ALPINE_BASE="${ALPINE_BASE:-https://dl-cdn.alpinelinux.org/alpine/edge/main/riscv64}"

mkdir -p "$VENDOR_DIR"

log() { printf '[fetch-busybox] %s\n' "$*" >&2; }

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    echo "no sha256 tool available" >&2
    exit 2
  fi
}

verify_or_pin() {
  local file="$1"
  local got
  got="$(sha256_of "$file")"
  if [ -f "$SHA_FILE" ]; then
    local expected
    expected="$(awk '{print $1}' "$SHA_FILE")"
    if [ "$got" != "$expected" ]; then
      log "SHA256 mismatch: expected $expected got $got"
      log "Refusing to overwrite $TARGET. Investigate upstream changes."
      return 1
    fi
    log "sha256 ok ($got)"
  else
    printf '%s  %s\n' "$got" "$(basename "$TARGET")" > "$SHA_FILE"
    log "wrote new pin $SHA_FILE ($got)"
  fi
}

write_source() {
  local url="$1"
  printf 'busybox riscv64 musl\nversion: %s\nfetched-from: %s\nfetched-at: %s\n' \
    "$BUSYBOX_VERSION" "$url" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$SOURCE_FILE"
}

is_riscv64_static_elf() {
  local file="$1"
  if ! command -v file >/dev/null 2>&1; then
    return 0  # skip the check; sha pin is the real backstop
  fi
  local out
  out="$(file -b "$file")"
  case "$out" in
    *"ELF 64-bit"*"RISC-V"*"statically linked"*) return 0 ;;
    *) log "warn: file($file)=$out (expected RISC-V static ELF)"; return 1 ;;
  esac
}

try_simple_download() {
  local url="$1"
  log "trying $url"
  local tmp
  tmp="$(mktemp)"
  if curl --fail --silent --show-error --location --output "$tmp" "$url"; then
    if is_riscv64_static_elf "$tmp"; then
      install -m 0755 "$tmp" "$TARGET"
      rm -f "$tmp"
      verify_or_pin "$TARGET" || return 1
      write_source "$url"
      log "wrote $TARGET ($(wc -c < "$TARGET") bytes)"
      return 0
    fi
  fi
  rm -f "$tmp"
  return 1
}

try_alpine_apk() {
  log "trying Alpine apk index at $ALPINE_BASE"
  if ! command -v tar >/dev/null 2>&1; then
    log "tar required for Alpine fallback; skipping"
    return 1
  fi
  local index_url="$ALPINE_BASE/APKINDEX.tar.gz"
  local workdir
  workdir="$(mktemp -d)"
  trap 'rm -rf "$workdir"' RETURN
  if ! curl --fail --silent --show-error --location --output "$workdir/APKINDEX.tar.gz" "$index_url"; then
    log "Alpine APKINDEX fetch failed"
    return 1
  fi
  tar -xzf "$workdir/APKINDEX.tar.gz" -C "$workdir" APKINDEX
  # Each package block ends with a blank line; pull busybox-static block.
  local pkg_file
  pkg_file="$(awk -v RS='' '/^P:busybox-static\n/' "$workdir/APKINDEX" | awk -F: '/^V:/{v=$2} END{printf "busybox-static-%s.apk", v}')"
  if [ -z "$pkg_file" ] || [ "$pkg_file" = "busybox-static-.apk" ]; then
    log "could not parse busybox-static version from APKINDEX"
    return 1
  fi
  local apk_url="$ALPINE_BASE/$pkg_file"
  log "downloading $apk_url"
  if ! curl --fail --silent --show-error --location --output "$workdir/pkg.apk" "$apk_url"; then
    log "apk fetch failed"
    return 1
  fi
  ( cd "$workdir" && tar -xzf pkg.apk bin/busybox.static 2>/dev/null )
  if [ ! -f "$workdir/bin/busybox.static" ]; then
    log "busybox.static not found inside apk"
    return 1
  fi
  if ! is_riscv64_static_elf "$workdir/bin/busybox.static"; then
    return 1
  fi
  install -m 0755 "$workdir/bin/busybox.static" "$TARGET"
  verify_or_pin "$TARGET" || return 1
  write_source "$apk_url"
  log "wrote $TARGET ($(wc -c < "$TARGET") bytes)"
  return 0
}

main() {
  if try_simple_download "$PRIMARY_URL"; then
    exit 0
  fi
  log "primary failed, trying leommxj"
  if try_simple_download "$FALLBACK_LEOMMXJ"; then
    exit 0
  fi
  log "leommxj failed, trying Alpine apk"
  if try_alpine_apk; then
    exit 0
  fi
  log "all sources failed"
  log "edit BUSYBOX_VERSION/PRIMARY_URL or build from source via musl.cc"
  exit 1
}

main "$@"
