#!/usr/bin/env bash
# Permanent, network-free LA64 SMP4 TLB-shootdown progress witness.
#
# Required inputs:
#   TX_LA64_IMAGE=/path/to/clean-la64-ext4.img
#   TX_LA64_PACK=/path/to/input.pack
#   TX_LA64_ORACLE_BIN=/path/to/precompiled-la64-oracle
#     OR
#   TX_LA64_CC='bash tools/images/loongarch64-linux-musl-gcc.zig-wrapper'
#
# Optional inputs:
#   TX_LA64_KERNEL=/path/to/kernel-la64-elf
#   TX_LA64_ROUNDS=8
#   TX_LA64_ORACLE_EPOCHS=32
#   TX_LA64_TIMEOUT_SECONDS=900
#   TX_LA64_ARTIFACT_PARENT=/tmp
#
# The source image is never attached to QEMU or modified. The pack and guest
# runner are injected into a mktemp-created image copy. This witness has no
# network device and never reads Git credentials or local launch-shell files.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEFAULT_KERNEL="$ROOT/target/loongarch64-unknown-none-softfloat/debug/tx-kernel-loongarch64-qemu-virt"
ORACLE_SOURCE="$ROOT/tools/shell-tests/la64-tlb-stale-map.c"

say() {
  printf '%s\n' "$*"
}

usage() {
  cat <<'EOF'
usage:
  TX_LA64_IMAGE=/path/to/clean-la64-ext4.img \
  TX_LA64_PACK=/path/to/input.pack \
  TX_LA64_ORACLE_BIN=/path/to/precompiled-la64-oracle \
    bash tools/la64-index-pack-shootdown-witness.sh

Instead of TX_LA64_ORACLE_BIN, set TX_LA64_CC to an explicit LA64 musl
compiler command. For example:
  TX_LA64_CC='bash tools/images/loongarch64-linux-musl-gcc.zig-wrapper'

The runner always uses LA64 QEMU with:
  -smp 4
  -accel tcg,thread=multi
  tx.maxcpus=4

The host timeout is a failure bound, never a success condition. Artifacts are
preserved under /tmp by default and their path is printed on every run.
EOF
}

if [[ -z "${TX_LA64_IMAGE:-}" || -z "${TX_LA64_PACK:-}" ]]; then
  usage >&2
  exit 2
fi
if [[ -n "${TX_LA64_ORACLE_BIN:-}" && -n "${TX_LA64_CC:-}" ]]; then
  say "FATAL: set exactly one of TX_LA64_ORACLE_BIN or TX_LA64_CC" >&2
  exit 2
fi
if [[ -z "${TX_LA64_ORACLE_BIN:-}" && -z "${TX_LA64_CC:-}" ]]; then
  say "FATAL: set TX_LA64_ORACLE_BIN or TX_LA64_CC for the stale-map oracle" >&2
  usage >&2
  exit 2
fi

IMAGE="$TX_LA64_IMAGE"
PACK="$TX_LA64_PACK"
KERNEL="${TX_LA64_KERNEL:-$DEFAULT_KERNEL}"
ROUNDS="${TX_LA64_ROUNDS:-8}"
ORACLE_EPOCHS="${TX_LA64_ORACLE_EPOCHS:-32}"
TIMEOUT_SECONDS="${TX_LA64_TIMEOUT_SECONDS:-900}"
ARTIFACT_PARENT="${TX_LA64_ARTIFACT_PARENT:-/tmp}"

case "$ROUNDS" in
  ''|*[!0-9]*|0)
    say "FATAL: TX_LA64_ROUNDS must be a positive integer" >&2
    exit 2
    ;;
esac
case "$ORACLE_EPOCHS" in
  ''|*[!0-9]*|0)
    say "FATAL: TX_LA64_ORACLE_EPOCHS must be a positive integer" >&2
    exit 2
    ;;
esac
case "$TIMEOUT_SECONDS" in
  ''|*[!0-9]*|0)
    say "FATAL: TX_LA64_TIMEOUT_SECONDS must be a positive integer" >&2
    exit 2
    ;;
esac

for command_name in cp debugfs file grep mktemp qemu-system-loongarch64 sed stat timeout tr; do
  if ! command -v "$command_name" >/dev/null 2>&1; then
    say "FATAL: required host command not found: $command_name" >&2
    exit 2
  fi
done

[[ -f "$IMAGE" ]] || { say "FATAL: TX_LA64_IMAGE is not a regular file: $IMAGE" >&2; exit 2; }
[[ -f "$PACK" ]] || { say "FATAL: TX_LA64_PACK is not a regular file: $PACK" >&2; exit 2; }
[[ -f "$ORACLE_SOURCE" ]] || { say "FATAL: stale-map oracle source not found: $ORACLE_SOURCE" >&2; exit 2; }
if [[ -n "${TX_LA64_ORACLE_BIN:-}" ]]; then
  [[ -f "$TX_LA64_ORACLE_BIN" ]] || {
    say "FATAL: TX_LA64_ORACLE_BIN is not a regular file: $TX_LA64_ORACLE_BIN" >&2
    exit 2
  }
fi
[[ -f "$KERNEL" ]] || {
  say "FATAL: LA64 kernel ELF not found: $KERNEL" >&2
  say "Build it with: cargo xtask build --target la64-qemu" >&2
  exit 2
}
[[ -d "$ARTIFACT_PARENT" ]] || {
  say "FATAL: TX_LA64_ARTIFACT_PARENT is not a directory: $ARTIFACT_PARENT" >&2
  exit 2
}

ARTIFACT_DIR="$(mktemp -d "$ARTIFACT_PARENT/la64-index-pack-witness-XXXXXX")" || exit 2
DISK="$ARTIFACT_DIR/disk.img"
GUEST_SCRIPT="$ARTIFACT_DIR/guest-witness.sh"
STAGED_PACK="$ARTIFACT_DIR/tlb-stress.pack"
ORACLE_BIN="$ARTIFACT_DIR/la64-tlb-stale-map"
ORACLE_SOURCE_COPY="$ARTIFACT_DIR/la64-tlb-stale-map.c"
ORACLE_BUILD_LOG="$ARTIFACT_DIR/oracle-build.log"
SERIAL_RAW="$ARTIFACT_DIR/serial.raw.log"
SERIAL="$ARTIFACT_DIR/serial.log"
QEMU_LOG="$ARTIFACT_DIR/qemu.log"
DEBUGFS_LOG="$ARTIFACT_DIR/debugfs.log"
FAILURE_MATCHES="$ARTIFACT_DIR/failure-matches.txt"
MANIFEST="$ARTIFACT_DIR/manifest.txt"

report_artifacts() {
  say "artifacts: $ARTIFACT_DIR"
}
trap report_artifacts EXIT

copy_with_reflink_fallback() {
  local source_file="$1"
  local destination_file="$2"
  local label="$3"

  if cp --reflink=always --sparse=always -- "$source_file" "$destination_file" 2>/dev/null; then
    say "$label copy: reflink"
  elif cp --sparse=always -- "$source_file" "$destination_file"; then
    say "$label copy: ordinary"
  else
    say "FATAL: failed to copy $label into artifact directory" >&2
    exit 2
  fi
}

copy_with_reflink_fallback "$IMAGE" "$DISK" "image"
copy_with_reflink_fallback "$PACK" "$STAGED_PACK" "pack"
copy_with_reflink_fallback "$ORACLE_SOURCE" "$ORACLE_SOURCE_COPY" "oracle source"

if [[ -n "${TX_LA64_ORACLE_BIN:-}" ]]; then
  copy_with_reflink_fallback "$TX_LA64_ORACLE_BIN" "$ORACLE_BIN" "oracle binary"
  ORACLE_INPUT="precompiled:$TX_LA64_ORACLE_BIN"
else
  read -r -a CC_COMMAND <<< "$TX_LA64_CC"
  if [[ "${#CC_COMMAND[@]}" -eq 0 ]] || ! command -v "${CC_COMMAND[0]}" >/dev/null 2>&1; then
    say "FATAL: TX_LA64_CC command not found: ${CC_COMMAND[0]:-$TX_LA64_CC}" >&2
    exit 2
  fi
  if ! "${CC_COMMAND[@]}" -static -O2 -std=c11 -pthread -Wall -Wextra -Werror \
    "$ORACLE_SOURCE_COPY" -o "$ORACLE_BIN" >"$ORACLE_BUILD_LOG" 2>&1; then
    say "FATAL: LA64 stale-map oracle compilation failed; see $ORACLE_BUILD_LOG" >&2
    exit 2
  fi
  ORACLE_INPUT="compiler:$TX_LA64_CC"
fi

ORACLE_FILE_DESCRIPTION="$(file -b "$ORACLE_BIN")"
if [[ "$ORACLE_FILE_DESCRIPTION" != *LoongArch* ]]; then
  say "FATAL: stale-map oracle is not a LoongArch binary: $ORACLE_FILE_DESCRIPTION" >&2
  exit 2
fi

cat > "$GUEST_SCRIPT" <<'GUESTEOF'
BB=/musl/bin/busybox
export HOME=/musl/root
export PATH=/musl/usr/bin:/musl/bin:/usr/bin:/bin
export GIT_EXEC_PATH=/musl/usr/libexec/git-core
export GIT_PAGER=cat
export GIT_CONFIG_NOSYSTEM=1
export GIT_TERMINAL_PROMPT=0

guest_fail() {
  echo "TLBWITNESS:FAIL:$1"
  $BB poweroff -f
  exit 1
}

echo "TLBWITNESS:BEGIN"
echo "TLBWITNESS:ROUNDS:__ROUNDS__"
echo "TLBWITNESS:ORACLE:EPOCHS:__ORACLE_EPOCHS__"

$BB chmod 755 /musl/root/la64-tlb-stale-map || guest_fail oracle-chmod
echo "TLBWITNESS:ORACLE:START"
/musl/root/la64-tlb-stale-map __ORACLE_EPOCHS__
oracle_rc=$?
echo "TLBWITNESS:ORACLE:RC:$oracle_rc"
[ "$oracle_rc" -eq 0 ] || guest_fail "oracle-rc-$oracle_rc"
echo "TLBWITNESS:ORACLE:PASS"

$BB rm -rf /musl/root/tlb-index-pack || guest_fail cleanup
$BB mkdir -p /musl/root/tlb-index-pack || guest_fail mkdir
cd /musl/root/tlb-index-pack || guest_fail chdir
git -c gc.auto=0 -c maintenance.auto=false init -q --bare . || guest_fail git-init

i=1
while [ "$i" -le __ROUNDS__ ]; do
  $BB rm -f objects/pack/*.pack objects/pack/*.idx objects/pack/*.rev || guest_fail "round-$i-clean"

  echo "TLBWITNESS:ROUND:$i:INDEX:START"
  $BB time git -c gc.auto=0 -c maintenance.auto=false \
    index-pack --stdin --fix-thin < /musl/root/tlb-stress.pack
  index_rc=$?
  [ "$index_rc" -eq 0 ] || guest_fail "round-$i-index-pack-rc-$index_rc"
  echo "TLBWITNESS:ROUND:$i:INDEX:PASS"

  echo "TLBWITNESS:ROUND:$i:FSCK:START"
  git -c gc.auto=0 -c maintenance.auto=false fsck --full --no-dangling
  fsck_rc=$?
  [ "$fsck_rc" -eq 0 ] || guest_fail "round-$i-fsck-rc-$fsck_rc"
  echo "TLBWITNESS:ROUND:$i:FSCK:PASS"
  echo "TLBWITNESS:ROUND:$i:PASS"

  i=$((i + 1))
done

echo "TLBWITNESS:PASS"
$BB poweroff -f
exit 0
GUESTEOF
sed -i "s/__ROUNDS__/$ROUNDS/g" "$GUEST_SCRIPT"
sed -i "s/__ORACLE_EPOCHS__/$ORACLE_EPOCHS/g" "$GUEST_SCRIPT"

# The ext4 image root is mounted at /musl in the guest: image `/foo` is guest
# `/musl/foo`. Remove stale destinations only inside the throwaway image.
# debugfs may return success for a missing path, so stat checks are authoritative.
debugfs -w -R "rm /tx-la64-index-pack-witness.sh" "$DISK" >>"$DEBUGFS_LOG" 2>&1 || true
debugfs -w -R "rm /root/tlb-stress.pack" "$DISK" >>"$DEBUGFS_LOG" 2>&1 || true
debugfs -w -R "rm /root/la64-tlb-stale-map" "$DISK" >>"$DEBUGFS_LOG" 2>&1 || true
debugfs -w -R "write $GUEST_SCRIPT /tx-la64-index-pack-witness.sh" "$DISK" >>"$DEBUGFS_LOG" 2>&1
debugfs -w -R "write $STAGED_PACK /root/tlb-stress.pack" "$DISK" >>"$DEBUGFS_LOG" 2>&1
debugfs -w -R "write $ORACLE_BIN /root/la64-tlb-stale-map" "$DISK" >>"$DEBUGFS_LOG" 2>&1

script_size="$(stat -c '%s' "$GUEST_SCRIPT")"
pack_size="$(stat -c '%s' "$STAGED_PACK")"
oracle_size="$(stat -c '%s' "$ORACLE_BIN")"
debugfs -R "stat /tx-la64-index-pack-witness.sh" "$DISK" >"$ARTIFACT_DIR/guest-script.stat" 2>>"$DEBUGFS_LOG"
debugfs -R "stat /root/tlb-stress.pack" "$DISK" >"$ARTIFACT_DIR/guest-pack.stat" 2>>"$DEBUGFS_LOG"
debugfs -R "stat /root/la64-tlb-stale-map" "$DISK" >"$ARTIFACT_DIR/guest-oracle.stat" 2>>"$DEBUGFS_LOG"
if ! grep -Eq "Size:[[:space:]]+$script_size([[:space:]]|$)" "$ARTIFACT_DIR/guest-script.stat"; then
  say "FATAL: guest script injection could not be verified" >&2
  exit 2
fi
if ! grep -Eq "Size:[[:space:]]+$pack_size([[:space:]]|$)" "$ARTIFACT_DIR/guest-pack.stat"; then
  say "FATAL: pack injection could not be verified" >&2
  exit 2
fi
if ! grep -Eq "Size:[[:space:]]+$oracle_size([[:space:]]|$)" "$ARTIFACT_DIR/guest-oracle.stat"; then
  say "FATAL: stale-map oracle injection could not be verified" >&2
  exit 2
fi

CMDLINE="tx.runsh=/musl/tx-la64-index-pack-witness.sh console=ttyS0 tx.maxcpus=4"
QEMU_COMMAND=(
  qemu-system-loongarch64
  -machine virt
  -cpu la464
  -kernel "$KERNEL"
  -m 1152M
  -smp 4
  -accel tcg,thread=multi
  -display none
  -monitor none
  -serial stdio
  -drive "file=$DISK,if=none,format=raw,id=x0,file.locking=off"
  -device virtio-blk-pci-non-transitional,drive=x0,rombar=0,addr=1
  -nic none
  -no-reboot
  -rtc base=utc
  -d guest_errors
  -D "$QEMU_LOG"
  -fw_cfg "name=opt/tx.cmdline,string=$CMDLINE"
  -append "$CMDLINE"
)

{
  printf 'image_source=%s\n' "$IMAGE"
  printf 'pack_source=%s\n' "$PACK"
  printf 'kernel=%s\n' "$KERNEL"
  printf 'rounds=%s\n' "$ROUNDS"
  printf 'oracle_epochs=%s\n' "$ORACLE_EPOCHS"
  printf 'oracle_source=%s\n' "$ORACLE_SOURCE"
  printf 'oracle_input=%s\n' "$ORACLE_INPUT"
  printf 'oracle_file=%s\n' "$ORACLE_FILE_DESCRIPTION"
  printf 'oracle_bytes=%s\n' "$oracle_size"
  printf 'timeout_seconds=%s\n' "$TIMEOUT_SECONDS"
  printf 'image_copy=%s\n' "$DISK"
  printf 'pack_bytes=%s\n' "$pack_size"
  printf 'qemu_smp=4\n'
  printf 'qemu_accel=tcg,thread=multi\n'
  printf 'kernel_cmdline=%s\n' "$CMDLINE"
  printf 'network=none\n'
  printf 'qemu_command='
  printf '%q ' "${QEMU_COMMAND[@]}"
  printf '\n'
} > "$MANIFEST"

say "== LA64 SMP4 offline index-pack shootdown witness =="
say "rounds: $ROUNDS"
say "stale-map oracle epochs: $ORACLE_EPOCHS"
say "host failure bound: ${TIMEOUT_SECONDS}s"
say "topology: -smp 4, tcg,thread=multi, tx.maxcpus=4"
say "network: disabled"
report_artifacts

timeout --signal=TERM --kill-after=10s "$TIMEOUT_SECONDS" \
  "${QEMU_COMMAND[@]}" >"$SERIAL_RAW" 2>"$ARTIFACT_DIR/qemu.stderr.log"
qemu_rc=$?
tr -d '\000\r' < "$SERIAL_RAW" > "$SERIAL"

: > "$FAILURE_MATCHES"
if grep -aEn \
  'TLBWITNESS:FAIL:|TLBORACLE:FAIL:|txkernel:la64-tlb-shootdown-stall|^txkernel:panic:|panicked at|^txkernel:[^[:space:]]*:trap$|^reason=trap-action-terminate$|invalid index-pack output' \
  "$SERIAL" > "$FAILURE_MATCHES"; then
  say "FAIL: guest failure or forbidden stall/panic/trap/index-pack signature found" >&2
  sed -n '1,80p' "$FAILURE_MATCHES" >&2
  exit 1
fi

if [[ "$qemu_rc" -eq 124 || "$qemu_rc" -eq 137 ]]; then
  say "FAIL: host timeout expired; timeout is never accepted as success" >&2
  tail -n 80 "$SERIAL" >&2
  exit 1
fi
if [[ "$qemu_rc" -ne 0 ]]; then
  say "FAIL: QEMU exited with status $qemu_rc" >&2
  tail -n 80 "$SERIAL" >&2
  exit 1
fi

if ! grep -aFq 'txkernel:qemu-loongarch64-virt:smp:cpus:possible=4:online-aps=3:online=4' "$SERIAL"; then
  say "FAIL: missing exact LA64 topology witness possible=4 / online=4" >&2
  tail -n 80 "$SERIAL" >&2
  exit 1
fi

if ! grep -aFxq 'TLBWITNESS:PASS' "$SERIAL"; then
  say "FAIL: guest did not publish the final success marker" >&2
  tail -n 80 "$SERIAL" >&2
  exit 1
fi

if [[ "$(grep -aFxc 'TLBWITNESS:ORACLE:PASS' "$SERIAL")" -ne 1 ]]; then
  say "FAIL: expected exactly one stale-map oracle wrapper success marker" >&2
  tail -n 80 "$SERIAL" >&2
  exit 1
fi
if [[ "$(grep -aFxc 'TLBORACLE:PASS' "$SERIAL")" -ne 1 ]]; then
  say "FAIL: expected exactly one stale-map oracle success marker" >&2
  tail -n 80 "$SERIAL" >&2
  exit 1
fi
oracle_result="TLBORACLE:RESULT:epochs=$ORACLE_EPOCHS:validated_writes=$ORACLE_EPOCHS:stale_writes=0:stale_reads=0:errors=0"
if [[ "$(grep -aFxc "$oracle_result" "$SERIAL")" -ne 1 ]]; then
  say "FAIL: stale-map oracle did not report all expected faults with zero stale access" >&2
  tail -n 80 "$SERIAL" >&2
  exit 1
fi

i=1
while [[ "$i" -le "$ROUNDS" ]]; do
  for phase in INDEX FSCK; do
    marker="TLBWITNESS:ROUND:$i:$phase:PASS"
    marker_count="$(grep -aFxc "$marker" "$SERIAL")"
    if [[ "$marker_count" -ne 1 ]]; then
      say "FAIL: expected exactly one '$marker', observed $marker_count" >&2
      exit 1
    fi
  done
  round_marker="TLBWITNESS:ROUND:$i:PASS"
  round_count="$(grep -aFxc "$round_marker" "$SERIAL")"
  if [[ "$round_count" -ne 1 ]]; then
    say "FAIL: expected exactly one '$round_marker', observed $round_count" >&2
    exit 1
  fi
  i=$((i + 1))
done

say "PASS: LA64 possible=4:online=4 completed $ORACLE_EPOCHS stale-map epochs and $ROUNDS index-pack + fsck rounds"
