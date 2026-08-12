#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
out="${1:?usage: build-riscv64-glibc-resolv-shim.sh OUT}"
if [[ -n "${CC:-}" ]]; then compiler=("$CC"); else compiler=(zig cc -target riscv64-linux-musl); fi
mkdir -p "$(dirname "$out")"
"${compiler[@]}" -shared -fPIC -Wl,-soname,libresolv.so.2 \
  -Wl,--version-script="$script_dir/riscv64-glibc-resolv-shim.map" \
  -o "$out" "$script_dir/riscv64-glibc-resolv-shim.c"
