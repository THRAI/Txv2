#!/usr/bin/env bash
# Fixed-topology BusyBox udhcpc regression fixture.
#
# VALIDATION-SCOPE WARNING:
# This fixture boots a tmpfs root and mounts the ext4 image as `/musl`.  Its
# optional Git clone is written under `/home` tmpfs, so the result validates
# DHCP/DNS/TLS/Git transport only.  It MUST NOT be cited as evidence for a
# direct-root ext4 RW mount, JBD2 durability, or survival across reboot.
#
# Every environment-specific input is replaceable:
#   TXKERNEL               kernel ELF
#   TX_DHCP_IMAGE          Alpine ext4 image containing udhcpc
#   TX_DHCP_QEMU_NET       QEMU user-network CIDR
#   TX_DHCP_QEMU_DHCPSTART expected first DHCP lease
#   TX_DHCP_TIMEOUT        host-side timeout in seconds
#   TX_DHCP_GIT_URL        optional public Git remote for a real shallow clone
#   TX_DHCP_GIT_TRACE      set to 1 to emit Git/libcurl trace to guest console
#   TX_DHCP_IFACE          required selector when the guest has multiple NICs

set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)"
KERNEL="${TXKERNEL:-$ROOT/target/riscv64gc-unknown-none-elf/debug/tx-kernel-riscv64-qemu-virt}"
IMAGE="${TX_DHCP_IMAGE:-$ROOT/local-images/alpine-linux-riscv64-ext4fs.img}"
QEMU_NET="${TX_DHCP_QEMU_NET:-172.31.44.0/24}"
DHCP_START="${TX_DHCP_QEMU_DHCPSTART:-172.31.44.20}"
HOST_TIMEOUT="${TX_DHCP_TIMEOUT:-120}"
GIT_URL="${TX_DHCP_GIT_URL:-}"
GIT_TRACE="${TX_DHCP_GIT_TRACE:-0}"
IFACE_SELECTOR="${TX_DHCP_IFACE:-}"
GUEST_PROBE="$ROOT/tools/udhcpc-guest-probe.sh"
WORK="$(mktemp -d /tmp/tx-udhcpc-rv64-XXXXXX)"
SERIAL="$WORK/serial.log"
NORMALIZED_SERIAL="$WORK/serial.normalized.log"

printf 'udhcpc witness workdir: %s\n' "$WORK"

if [[ "$GIT_URL" =~ [[:space:]] ]] || [[ "$IFACE_SELECTOR" =~ [[:space:]] ]]; then
    printf 'FATAL: Git URL and interface selector must not contain whitespace\n' >&2
    exit 2
fi
GIT_CMDLINE=""
if [ -n "$GIT_URL" ]; then
    case "$GIT_URL" in
        http://* | https://*) ;;
        *)
            printf 'FATAL: TX_DHCP_GIT_URL must be an HTTP(S) public remote\n' >&2
            exit 2
            ;;
    esac
    if [[ "${GIT_URL#*://}" == *@* ]]; then
        printf 'FATAL: credentials in TX_DHCP_GIT_URL are forbidden\n' >&2
        exit 2
    fi
    GIT_CMDLINE=" tx.git.remote=$GIT_URL"
fi
if [ "$GIT_TRACE" = "1" ]; then
    GIT_CMDLINE="$GIT_CMDLINE tx.git.trace=1"
fi
if [ -n "$IFACE_SELECTOR" ]; then
    GIT_CMDLINE="$GIT_CMDLINE tx.net.iface=$IFACE_SELECTOR"
fi

for required in "$KERNEL" "$IMAGE" "$GUEST_PROBE"; do
    if [ ! -f "$required" ]; then
        printf 'FATAL: required file not found: %s\n' "$required" >&2
        exit 2
    fi
done

for command_name in qemu-system-riscv64 debugfs timeout; do
    if ! command -v "$command_name" >/dev/null 2>&1; then
        printf 'FATAL: required command not found: %s\n' "$command_name" >&2
        exit 2
    fi
done

cp "$IMAGE" "$WORK/disk.img"
debugfs -w -R "write $GUEST_PROBE /tx-udhcpc-probe.sh" "$WORK/disk.img" >/dev/null 2>&1

set +e
timeout "$HOST_TIMEOUT" qemu-system-riscv64 \
    -machine virt \
    -kernel "$KERNEL" \
    -m 1G \
    -nographic \
    -smp 1 \
    -bios default \
    -global virtio-mmio.force-legacy=false \
    -drive "file=$WORK/disk.img,if=none,format=raw,id=x0,file.locking=off" \
    -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
    -device virtio-net-device,netdev=net,bus=virtio-mmio-bus.1 \
    -netdev "user,id=net,net=$QEMU_NET,dhcpstart=$DHCP_START" \
    -no-reboot \
    -rtc base=utc \
    -append "tx.runsh=/musl/tx-udhcpc-probe.sh tx.net.mode=dhcp${GIT_CMDLINE} console=ttyS0" \
    >"$SERIAL" 2>&1
qemu_rc=$?
set -e

tr -d '\r' <"$SERIAL" >"$NORMALIZED_SERIAL"
grep -a '^TXDHCP:' "$NORMALIZED_SERIAL" || true

if grep -aq '^TXDHCP:lease-result:pass$' "$NORMALIZED_SERIAL" \
    && grep -aq "^TXDHCP:lease:$DHCP_START$" "$NORMALIZED_SERIAL"; then
    printf 'PASS: udhcpc acquired the configured QEMU lease %s\n' "$DHCP_START"
    if grep -aq '^TXDHCP:gateway-result:pass$' "$NORMALIZED_SERIAL"; then
        printf 'PASS: the configured gateway is reachable\n'
    else
        printf 'PARTIAL: lease succeeded, but the configured gateway is not reachable\n'
    fi
    if grep -aq '^TXDHCP:dns-result:pass$' "$NORMALIZED_SERIAL"; then
        printf 'PASS: DHCP-provided DNS resolves github.com\n'
    else
        printf 'PARTIAL: lease succeeded, but DNS resolution failed\n'
    fi
    if [ -n "$GIT_URL" ]; then
        if grep -aq '^TXDHCP:git-result:pass$' "$NORMALIZED_SERIAL"; then
            printf 'PASS: real shallow clone completed from %s\n' "$GIT_URL"
        else
            printf 'FAIL: real shallow clone failed for %s\n' "$GIT_URL" >&2
            printf 'serial log: %s\n' "$SERIAL" >&2
            exit 1
        fi
    fi
    exit 0
fi

printf 'FAIL: udhcpc did not acquire expected lease %s (qemu rc=%s)\n' "$DHCP_START" "$qemu_rc" >&2
printf 'serial log: %s\n' "$SERIAL" >&2
exit 1
