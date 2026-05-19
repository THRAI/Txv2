#!/usr/bin/env bash
# Prepare a small Alpine riscv64 rootfs for txKernel userspace ABI probes.
#
# The output is intentionally under target/rootfs so the downloaded rootfs and
# extracted APK payloads stay out of git. This script does not chroot or run any
# guest binaries on the host; it downloads Alpine packages and extracts them.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/../.." && pwd)"

ARCH="${ALPINE_ARCH:-riscv64}"
BRANCH="${ALPINE_BRANCH:-latest-stable}"
MIRROR="${ALPINE_MIRROR:-https://dl-cdn.alpinelinux.org/alpine}"
ROOTFS_DIR="${TX_ALPINE_ROOTFS:-$REPO_ROOT/target/rootfs/alpine-rv64-qemu}"
CACHE_DIR="${TX_ALPINE_CACHE:-$REPO_ROOT/target/images/alpine-cache}"
PACKAGES="${TX_ALPINE_PACKAGES:-busybox nftables iptables iproute2}"
REPOS="${TX_ALPINE_REPOS:-main community}"
MINIROOTFS_URL="${ALPINE_MINIROOTFS_URL:-}"

log() {
  printf '[fetch-alpine-rv64] %s\n' "$*" >&2
}

die() {
  printf '[fetch-alpine-rv64] error: %s\n' "$*" >&2
  exit 1
}

download() {
  local url="$1"
  local out="$2"
  if [ -s "$out" ]; then
    return 0
  fi
  mkdir -p "$(dirname -- "$out")"
  log "download $url"
  if command -v curl >/dev/null 2>&1; then
    curl -fL --retry 3 --connect-timeout 20 -o "$out.tmp" "$url"
  elif command -v wget >/dev/null 2>&1; then
    wget -O "$out.tmp" "$url"
  else
    die "need curl or wget"
  fi
  mv "$out.tmp" "$out"
}

safe_reset_dir() {
  local dir="$1"
  case "$dir" in
    ""|"/"|"$REPO_ROOT"|"$REPO_ROOT/"|"."|"..")
      die "refusing to remove unsafe rootfs path: $dir"
      ;;
  esac
  rm -rf -- "$dir"
  mkdir -p -- "$dir"
}

default_minirootfs_url() {
  local latest="$CACHE_DIR/latest-releases.yaml"
  local latest_url="$MIRROR/$BRANCH/releases/$ARCH/latest-releases.yaml"
  download "$latest_url" "$latest"
  local file
  file="$(
    awk '
      $1 == "file:" && $2 ~ /^alpine-minirootfs-.*-riscv64\.tar\.gz$/ {
        print $2;
        exit;
      }
    ' "$latest"
  )"
  if [ -z "$file" ]; then
    die "could not discover alpine minirootfs from $latest_url"
  fi
  printf '%s/releases/%s/%s\n' "$MIRROR/$BRANCH" "$ARCH" "$file"
}

fetch_index() {
  local repo="$1"
  local archive="$CACHE_DIR/$repo-APKINDEX.tar.gz"
  local text="$CACHE_DIR/$repo-APKINDEX"
  download "$MIRROR/$BRANCH/$repo/$ARCH/APKINDEX.tar.gz" "$archive"
  tar -xOzf "$archive" APKINDEX > "$text"
}

record_for_pkg() {
  local repo="$1"
  local pkg="$2"
  awk -v pkg="$pkg" '
    BEGIN { RS = ""; FS = "\n" }
    {
      found = 0;
      for (i = 1; i <= NF; i++) {
        if ($i == "P:" pkg) {
          found = 1;
          break;
        }
      }
      if (found) {
        print $0;
        exit;
      }
    }
  ' "$CACHE_DIR/$repo-APKINDEX"
}

field_from_record() {
  local field="$1"
  awk -v field="$field" -F: '$1 == field { print substr($0, length(field) + 2); exit }'
}

normalize_dep() {
  local dep="$1"
  dep="${dep%%[<>=~]*}"
  printf '%s\n' "$dep"
}

find_pkg_for_token() {
  local token="$1"
  local repo pkg
  for repo in $REPOS; do
    pkg="$(
      awk -v token="$token" '
        BEGIN { RS = ""; FS = "\n" }
        {
          pkg = "";
          provides = "";
          for (i = 1; i <= NF; i++) {
            if ($i ~ /^P:/) pkg = substr($i, 3);
            if ($i ~ /^p:/) provides = substr($i, 3);
          }
          if (pkg == token) {
            print pkg;
            exit;
          }
          n = split(provides, p, " ");
          for (j = 1; j <= n; j++) {
            provided = p[j];
            sub(/[<>=~].*$/, "", provided);
            if (provided == token) {
              print pkg;
              exit;
            }
          }
        }
      ' "$CACHE_DIR/$repo-APKINDEX"
    )"
    if [ -n "$pkg" ]; then
      printf '%s:%s\n' "$repo" "$pkg"
      return 0
    fi
  done
  return 1
}

declare -A SEEN=()
declare -a QUEUE=()
declare -a ORDER=()

enqueue_pkg() {
  local repo_pkg="$1"
  local repo="${repo_pkg%%:*}"
  local pkg="${repo_pkg#*:}"
  local key="$repo:$pkg"
  if [ -n "${SEEN[$key]:-}" ]; then
    return 0
  fi
  SEEN["$key"]=1
  QUEUE+=("$key")
  ORDER+=("$key")
}

resolve_package_closure() {
  local pkg token repo_pkg dep record deps idx current repo name
  for pkg in $PACKAGES; do
    token="$(normalize_dep "$pkg")"
    repo_pkg="$(find_pkg_for_token "$token")" || die "package not found: $pkg"
    enqueue_pkg "$repo_pkg"
  done

  idx=0
  while [ "$idx" -lt "${#QUEUE[@]}" ]; do
    current="${QUEUE[$idx]}"
    idx=$((idx + 1))
    repo="${current%%:*}"
    name="${current#*:}"
    record="$(record_for_pkg "$repo" "$name")"
    deps="$(printf '%s\n' "$record" | field_from_record D || true)"
    for dep in $deps; do
      case "$dep" in
        ""|/*|!*) continue ;;
      esac
      token="$(normalize_dep "$dep")"
      repo_pkg="$(find_pkg_for_token "$token")" || {
        log "warn: unresolved dependency token $dep for $name"
        continue
      }
      enqueue_pkg "$repo_pkg"
    done
  done
}

install_apk() {
  local repo="$1"
  local pkg="$2"
  local record version url apk
  record="$(record_for_pkg "$repo" "$pkg")"
  version="$(printf '%s\n' "$record" | field_from_record V)"
  [ -n "$version" ] || die "missing version for $repo/$pkg"
  apk="$CACHE_DIR/apks/$pkg-$version.apk"
  url="$MIRROR/$BRANCH/$repo/$ARCH/$pkg-$version.apk"
  download "$url" "$apk"
  log "extract $pkg-$version.apk"
  tar -xzf "$apk" -C "$ROOTFS_DIR" \
    --exclude='.SIGN.*' \
    --exclude='.PKGINFO' \
    --exclude='.INSTALL' \
    --exclude='.pre-install' \
    --exclude='.post-install' \
    --exclude='.pre-upgrade' \
    --exclude='.post-upgrade'
}

main() {
  [ "$ARCH" = "riscv64" ] || die "this helper is for riscv64, got $ARCH"
  mkdir -p "$CACHE_DIR"

  if [ -z "$MINIROOTFS_URL" ]; then
    MINIROOTFS_URL="$(default_minirootfs_url)"
  fi
  local mini="$CACHE_DIR/${MINIROOTFS_URL##*/}"
  download "$MINIROOTFS_URL" "$mini"

  log "reset $ROOTFS_DIR"
  safe_reset_dir "$ROOTFS_DIR"
  tar -xzf "$mini" -C "$ROOTFS_DIR"

  local repo
  for repo in $REPOS; do
    fetch_index "$repo"
  done

  resolve_package_closure
  for item in "${ORDER[@]}"; do
    install_apk "${item%%:*}" "${item#*:}"
  done

  mkdir -p "$ROOTFS_DIR"/dev "$ROOTFS_DIR"/proc "$ROOTFS_DIR"/sys \
    "$ROOTFS_DIR"/tmp "$ROOTFS_DIR"/run "$ROOTFS_DIR"/var/run
  chmod 1777 "$ROOTFS_DIR"/tmp
  mkdir -p "$ROOTFS_DIR/etc/apk" "$ROOTFS_DIR/var/lib"
  {
    for repo in $REPOS; do
      printf '%s/%s/%s/%s\n' "$MIRROR" "$BRANCH" "$repo" "$ARCH"
    done
  } > "$ROOTFS_DIR/etc/apk/repositories"
  {
    printf 'rootfs: %s\n' "$MINIROOTFS_URL"
    printf 'packages: %s\n' "$PACKAGES"
    printf 'resolved:\n'
    for item in "${ORDER[@]}"; do
      printf '  - %s\n' "$item"
    done
  } > "$ROOTFS_DIR/var/lib/tx-alpine-packages.txt"

  log "ready: $ROOTFS_DIR"
  log "build image: TX_ALPINE_ROOTFS=$ROOTFS_DIR cargo xtask image cpio --profile alpine --target rv64-qemu"
}

main "$@"
