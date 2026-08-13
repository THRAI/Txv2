#!/usr/bin/env bash
set -euo pipefail

# This receives the copied toolchain staged for WORKLOAD, never an Alpine rootfs.
toolchain="${1:?usage: install-riscv64-rustc-resolv-shim.sh WORKLOAD_TOOLCHAIN}"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
[[ -f "$toolchain/bin/rustc" ]] || { echo "missing copied rustc: $toolchain/bin/rustc" >&2; exit 1; }
[[ -f "$toolchain/bin/cargo" ]] || { echo "missing copied cargo: $toolchain/bin/cargo" >&2; exit 1; }
"$script_dir/build-riscv64-glibc-resolv-shim.sh" "$toolchain/lib/libtx-rustc-resolv-preload.so"
"$script_dir/patch-riscv64-glibc-interpreter.py" --allow-noop "$toolchain"
