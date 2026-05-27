#!/usr/bin/env bash
# Build a Txv2-owned static LoongArch64 BusyBox and install it under
# tools/images/vendor/busybox-loongarch64-musl.
#
# Usage:
#   tools/images/build-busybox-loongarch64.sh
#   BUSYBOX_SRC=/path/to/busybox tools/images/build-busybox-loongarch64.sh
#   BUSYBOX_VERSION=1.36.1 tools/images/build-busybox-loongarch64.sh
#
# Requirements:
#   - loongarch64-linux-musl-gcc and matching binutils in PATH
#   - make
#   - curl + tar if BUSYBOX_SRC is not provided and the source is not cached

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
VENDOR_DIR="$SCRIPT_DIR/vendor"
OUT="$VENDOR_DIR/busybox-loongarch64-musl"
SHA_FILE="$OUT.sha256"
SOURCE_FILE="$OUT.SOURCE"

BUSYBOX_VERSION="${BUSYBOX_VERSION:-1.36.1}"
CROSS_COMPILE="${CROSS_COMPILE:-loongarch64-linux-musl-}"
JOBS="${JOBS:-$(nproc 2>/dev/null || echo 1)}"

log() { printf '[build-busybox-la64] %s\n' "$*" >&2; }

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    log "missing required tool: $1"
    exit 2
  fi
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    log "missing sha256sum or shasum"
    exit 2
  fi
}

resolve_source() {
  if [ -n "${BUSYBOX_SRC:-}" ]; then
    if [ ! -d "$BUSYBOX_SRC" ]; then
      log "BUSYBOX_SRC is not a directory: $BUSYBOX_SRC"
      exit 2
    fi
    printf '%s\n' "$BUSYBOX_SRC"
    return
  fi

  need curl
  need tar

  local sources_dir="$ROOT/target/sources"
  local src="$sources_dir/busybox-$BUSYBOX_VERSION"
  if [ -d "$src" ]; then
    printf '%s\n' "$src"
    return
  fi

  mkdir -p "$sources_dir"
  local archive="$sources_dir/busybox-$BUSYBOX_VERSION.tar.bz2"
  local url="https://busybox.net/downloads/busybox-$BUSYBOX_VERSION.tar.bz2"
  log "downloading $url"
  curl --fail --location --output "$archive" "$url"
  log "extracting $archive"
  tar -xjf "$archive" -C "$sources_dir"
  printf '%s\n' "$src"
}

write_source() {
  local src="$1"
  printf 'busybox loongarch64 musl\nversion: %s\nsource: %s\ncross-compile: %s\nbuilt-at: %s\n' \
    "$BUSYBOX_VERSION" "$src" "$CROSS_COMPILE" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$SOURCE_FILE"
}

set_config() {
  local file="$1"
  local key="$2"
  local value="$3"
  if grep -q -E "^(# )?${key}[ =]" "$file"; then
    sed -i -E "s|^(# )?${key}[ =].*|${key}=${value}|" "$file"
  else
    printf '%s=%s\n' "$key" "$value" >> "$file"
  fi
}

unset_config() {
  local file="$1"
  local key="$2"
  if grep -q -E "^(# )?${key}[ =]" "$file"; then
    sed -i -E "s|^(# )?${key}[ =].*|# ${key} is not set|" "$file"
  else
    printf '# %s is not set\n' "$key" >> "$file"
  fi
}

main() {
  need make
  if ! command -v "${CROSS_COMPILE}gcc" >/dev/null 2>&1; then
    if command -v "${CROSS_COMPILE}cc" >/dev/null 2>&1; then
      log "using ${CROSS_COMPILE}cc as ${CROSS_COMPILE}gcc"
      mkdir -p "$ROOT/target/toolchain-wrappers"
      printf '#!/usr/bin/env sh\nexec %scc "$@"\n' "$CROSS_COMPILE" \
        > "$ROOT/target/toolchain-wrappers/${CROSS_COMPILE}gcc"
      chmod +x "$ROOT/target/toolchain-wrappers/${CROSS_COMPILE}gcc"
      export PATH="$ROOT/target/toolchain-wrappers:$PATH"
    else
      need "${CROSS_COMPILE}gcc"
    fi
  fi
  need file

  local src
  src="$(resolve_source)"
  mkdir -p "$VENDOR_DIR"

  log "building minimal static busybox from $src"
  make -C "$src" ARCH=loongarch CROSS_COMPILE="$CROSS_COMPILE" distclean
  make -C "$src" ARCH=loongarch CROSS_COMPILE="$CROSS_COMPILE" allnoconfig

  set_config "$src/.config" CONFIG_SHOW_USAGE y
  set_config "$src/.config" CONFIG_LONG_OPTS y
  set_config "$src/.config" CONFIG_LFS y
  set_config "$src/.config" CONFIG_BUSYBOX y
  set_config "$src/.config" CONFIG_BUSYBOX_EXEC_PATH '"/bin/busybox"'
  set_config "$src/.config" CONFIG_STATIC y
  set_config "$src/.config" CONFIG_STATIC_LIBGCC y
  unset_config "$src/.config" CONFIG_BUILD_LIBBUSYBOX
  unset_config "$src/.config" CONFIG_FEATURE_PREFER_APPLETS

  set_config "$src/.config" CONFIG_FEATURE_EDITING y
  set_config "$src/.config" CONFIG_FEATURE_EDITING_MAX_LEN 1024
  set_config "$src/.config" CONFIG_FEATURE_EDITING_HISTORY 64
  set_config "$src/.config" CONFIG_FEATURE_EDITING_FANCY_PROMPT y
  set_config "$src/.config" CONFIG_FEATURE_TAB_COMPLETION y

  set_config "$src/.config" CONFIG_SH_IS_ASH y
  set_config "$src/.config" CONFIG_BASH_IS_NONE y
  set_config "$src/.config" CONFIG_ASH y
  set_config "$src/.config" CONFIG_ASH_OPTIMIZE_FOR_SIZE y
  set_config "$src/.config" CONFIG_ASH_INTERNAL_GLOB y
  set_config "$src/.config" CONFIG_ASH_ALIAS y
  set_config "$src/.config" CONFIG_ASH_ECHO y
  set_config "$src/.config" CONFIG_ASH_PRINTF y
  set_config "$src/.config" CONFIG_ASH_TEST y
  set_config "$src/.config" CONFIG_ASH_SLEEP y
  set_config "$src/.config" CONFIG_ASH_HELP y
  set_config "$src/.config" CONFIG_ASH_CMDCMD y
  unset_config "$src/.config" CONFIG_FEATURE_SH_STANDALONE

  set_config "$src/.config" CONFIG_CAT y
  set_config "$src/.config" CONFIG_ECHO y
  set_config "$src/.config" CONFIG_FEATURE_FANCY_ECHO y
  set_config "$src/.config" CONFIG_FALSE y
  set_config "$src/.config" CONFIG_LS y
  set_config "$src/.config" CONFIG_MKDIR y
  set_config "$src/.config" CONFIG_MOUNT y
  set_config "$src/.config" CONFIG_PWD y
  set_config "$src/.config" CONFIG_RM y
  set_config "$src/.config" CONFIG_RMDIR y
  set_config "$src/.config" CONFIG_TRUE y
  set +o pipefail
  yes '' | make -C "$src" ARCH=loongarch CROSS_COMPILE="$CROSS_COMPILE" oldconfig
  oldconfig_status="${PIPESTATUS[1]}"
  set -o pipefail
  if [ "$oldconfig_status" -ne 0 ]; then
    exit "$oldconfig_status"
  fi
  make -C "$src" ARCH=loongarch CROSS_COMPILE="$CROSS_COMPILE" SKIP_STRIP=y -j"$JOBS" busybox

  install -m 0755 "$src/busybox" "$OUT"

  local file_out
  file_out="$(file -b "$OUT")"
  case "$file_out" in
    *"ELF 64-bit"*"LoongArch"*"statically linked"*) ;;
    *)
      log "unexpected output binary: $file_out"
      exit 1
      ;;
  esac

  printf '%s  %s\n' "$(sha256_of "$OUT")" "$(basename "$OUT")" > "$SHA_FILE"
  write_source "$src"
  log "wrote $OUT"
  log "file: $file_out"
}

main "$@"
