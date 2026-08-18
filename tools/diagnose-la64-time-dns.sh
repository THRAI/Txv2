#!/usr/bin/env bash
# Focused LA64 witness for separating deadline/timeout failures from DNS/UDP.
#
# Usage:
#   bash tools/diagnose-la64-time-dns.sh clock
#   bash tools/diagnose-la64-time-dns.sh timer
#   bash tools/diagnose-la64-time-dns.sh dns
#   bash tools/diagnose-la64-time-dns.sh resolver
#   bash tools/diagnose-la64-time-dns.sh git-dns
#   bash tools/diagnose-la64-time-dns.sh git-dns-trace
#   bash tools/diagnose-la64-time-dns.sh github-ls-remote
#   bash tools/diagnose-la64-time-dns.sh github-clone
#   bash tools/diagnose-la64-time-dns.sh github-shallow-clone
# Optional:
#   TX_LA64_FOCUS_TIMEOUT=60 bash tools/diagnose-la64-time-dns.sh dns
#   TX_FOCUS_ARCH=rv64 bash tools/diagnose-la64-time-dns.sh connect-timeout
#   TX_LA64_FOCUS_MONITOR=1 bash tools/diagnose-la64-time-dns.sh github-ls-remote
#   TX_LA64_FOCUS_QEMU_TRACE=1 bash tools/diagnose-la64-time-dns.sh github-ls-remote
set -u

MODE="${1:-timer}"
ARCH="${TX_FOCUS_ARCH:-la64}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d /tmp/la64-focus-XXXXXX)"
SERIAL="$WORK/serial.log"
cleanup() { rm -f "$WORK/disk.img"; }
trap cleanup EXIT
if [ "$MODE" = "github-clone" ] || [ "$MODE" = "github-shallow-clone" ]; then
  DEFAULT_QEMU_TIMEOUT=240
else
  DEFAULT_QEMU_TIMEOUT=45
fi
QEMU_TIMEOUT="${TX_LA64_FOCUS_TIMEOUT:-$DEFAULT_QEMU_TIMEOUT}"
MONITOR_ARGS=()
MONITOR_ENABLED="${TX_LA64_FOCUS_MONITOR:-0}"
if [ "$MONITOR_ENABLED" = "1" ] && [ "$ARCH" = "la64" ]; then
  MONITOR_SOCKET="$WORK/monitor.sock"
  MONITOR_LOG="$WORK/monitor.log"
  MONITOR_ARGS=(-monitor "unix:$MONITOR_SOCKET,server=on,wait=off")
  echo "monitor socket: $MONITOR_SOCKET"
  echo "monitor log: $MONITOR_LOG"
fi
TRACE_ARGS=()
if [ "${TX_LA64_FOCUS_QEMU_TRACE:-0}" = "1" ] && [ "$ARCH" = "la64" ]; then
  QEMU_TRACE_LOG="$WORK/qemu-trace.log"
  TRACE_ARGS=(
    -trace enable=pci_route_irq
    -trace enable=loongarch_pch_pic_irq_handler
    -trace enable=loongarch_pch_pic_low_writew
    -trace enable=loongarch_extioi_setirq
    -trace enable=loongarch_extioi_writew
    -D "$QEMU_TRACE_LOG"
  )
  echo "qemu trace: $QEMU_TRACE_LOG"
fi

case "$ARCH" in
  la64)
    K="$ROOT/target/loongarch64-unknown-none-softfloat/debug/tx-kernel-loongarch64-qemu-virt"
    IMG="$ROOT/local-images/alpine-linux-loongarch64-ext4fs.img"
    ;;
  rv64)
    K="$ROOT/target/riscv64gc-unknown-none-elf/debug/tx-kernel-riscv64-qemu-virt"
    IMG="$ROOT/local-images/alpine-linux-riscv64-ext4fs.img"
    ;;
  *)
    echo "unsupported TX_FOCUS_ARCH: $ARCH"
    exit 2
    ;;
esac

[ -f "$K" ] || { echo "kernel missing: $K"; exit 2; }
[ -f "$IMG" ] || { echo "image missing: $IMG"; exit 2; }

case "$MODE" in
  clock)
    cat > "$WORK/tx-run.sh" <<'GUESTEOF'
BB=/musl/bin/busybox
echo "LAFOCUS:clock:date:[$($BB date -u 2>&1)]"
echo "LAFOCUS:clock:epoch:[$($BB date +%s 2>&1)]"
echo "LAFOCUS:END"
GUESTEOF
    ;;
  timer)
    cat > "$WORK/tx-run.sh" <<'GUESTEOF'
BB=/musl/bin/busybox
echo "LAFOCUS:timer:begin"
echo "LAFOCUS:sleep:begin"
$BB sleep 1
echo "LAFOCUS:sleep:rc:$?"
echo "LAFOCUS:timeout:begin"
$BB timeout 2 $BB sleep 30
echo "LAFOCUS:timeout:rc:$?"
echo "LAFOCUS:signalwait:begin"
$BB sleep 30 &
child=$!
$BB sleep 1
$BB kill -TERM "$child"
wait "$child"
echo "LAFOCUS:signalwait:rc:$?"
echo "LAFOCUS:END"
GUESTEOF
    ;;
  dns)
    cat > "$WORK/tx-run.sh" <<'GUESTEOF'
BB=/musl/bin/busybox
$BB mkdir -p /etc 2>/dev/null
echo "nameserver 10.0.2.3" > /etc/resolv.conf
echo "LAFOCUS:dns:begin"
$BB nslookup example.com 10.0.2.3
echo "LAFOCUS:dns:rc:$?"
echo "LAFOCUS:END"
GUESTEOF
    ;;
  resolver)
    cat > "$WORK/tx-run.sh" <<'GUESTEOF'
BB=/musl/bin/busybox
$BB mkdir -p /etc 2>/dev/null
echo "nameserver 10.0.2.3" > /etc/resolv.conf
echo "LAFOCUS:resolver:numeric:begin"
$BB timeout 8 $BB ping -c 1 -W 2 10.0.2.2
echo "LAFOCUS:resolver:numeric:rc:$?"
echo "LAFOCUS:resolver:hostname:begin"
$BB timeout 12 $BB ping -c 1 -W 2 example.com
echo "LAFOCUS:resolver:hostname:rc:$?"
echo "LAFOCUS:END"
GUESTEOF
    ;;
  git-dns)
    cat > "$WORK/tx-run.sh" <<'GUESTEOF'
BB=/musl/bin/busybox
export GIT_PAGER=cat HOME=/musl/root GIT_EXEC_PATH=/musl/usr/libexec/git-core \
       GIT_TEMPLATE_DIR= GIT_SSL_NO_VERIFY=true GIT_CURL_VERBOSE=1 \
       PATH=/musl/usr/bin:/musl/bin:/usr/bin:/bin
G="git -c gc.auto=0 -c maintenance.auto=false -c http.sslVerify=false"
$BB mkdir -p /etc 2>/dev/null
echo "nameserver 10.0.2.3" > /etc/resolv.conf
echo "LAFOCUS:git-dns:begin"
$BB timeout 30 $G ls-remote https://github.com:19999/x.git >/tmp/git-dns.txt 2>&1
echo "LAFOCUS:git-dns:rc:$?"
echo "LAFOCUS:git-dns:evidence:[$($BB grep -aiE 'was resolved|Trying [0-9]' /tmp/git-dns.txt | $BB head -1)]"
echo "LAFOCUS:END"
GUESTEOF
    ;;
  git-dns-trace)
    cat > "$WORK/tx-run.sh" <<'GUESTEOF'
BB=/musl/bin/busybox
export GIT_PAGER=cat HOME=/musl/root GIT_EXEC_PATH=/musl/usr/libexec/git-core \
       GIT_TEMPLATE_DIR= GIT_SSL_NO_VERIFY=true GIT_CURL_VERBOSE=1 \
       GIT_TRACE=1 GIT_TRACE_CURL=1 \
       PATH=/musl/usr/bin:/musl/bin:/usr/bin:/bin
G="git -c gc.auto=0 -c maintenance.auto=false -c http.sslVerify=false"
$BB mkdir -p /etc 2>/dev/null
echo "nameserver 10.0.2.3" > /etc/resolv.conf
echo "LAFOCUS:git-dns-trace:begin"
$BB timeout 20 $G ls-remote https://github.com:19999/x.git
echo "LAFOCUS:git-dns-trace:rc:$?"
echo "LAFOCUS:END"
GUESTEOF
    ;;
  github-ls-remote)
    cat > "$WORK/tx-run.sh" <<'GUESTEOF'
BB=/musl/bin/busybox
export GIT_PAGER=cat HOME=/musl/root GIT_EXEC_PATH=/musl/usr/libexec/git-core \
       GIT_TEMPLATE_DIR= GIT_TRACE=1 GIT_TRACE_CURL=1 \
       GIT_TRACE_CURL_NO_DATA=1 GIT_TRACE_PACKET=1 \
       PATH=/musl/usr/bin:/musl/bin:/usr/bin:/bin
G="git -c gc.auto=0 -c maintenance.auto=false -c http.sslVerify=true -c http.proxy=http://10.0.2.2:7897"
$BB mkdir -p /etc 2>/dev/null
echo "nameserver 10.0.2.3" > /etc/resolv.conf
if [ ! -e /etc/ssl ]; then
    $BB ln -s /musl/etc/ssl /etc/ssl
fi
echo "LAFOCUS:github-ls-remote:date:[$($BB date -u 2>&1)]"
echo "LAFOCUS:github-ls-remote:dns:begin"
$BB nslookup github.com 10.0.2.3
echo "LAFOCUS:github-ls-remote:dns:rc:$?"
echo "LAFOCUS:github-ls-remote:git:begin"
$BB timeout 45 $G ls-remote https://github.com/oscomp/xv6-riscv.git HEAD
echo "LAFOCUS:github-ls-remote:git:rc:$?"
echo "LAFOCUS:END"
GUESTEOF
    ;;
  github-clone)
    cat > "$WORK/tx-run.sh" <<'GUESTEOF'
BB=/musl/bin/busybox
export GIT_PAGER=cat HOME=/musl/root GIT_EXEC_PATH=/musl/usr/libexec/git-core \
       GIT_TEMPLATE_DIR= PATH=/musl/usr/bin:/musl/bin:/usr/bin:/bin
G="git -c gc.auto=0 -c maintenance.auto=false -c http.sslVerify=true -c http.proxy=http://10.0.2.2:7897"
$BB mkdir -p /etc 2>/dev/null
echo "nameserver 10.0.2.3" > /etc/resolv.conf
if [ ! -e /etc/ssl ]; then
    $BB ln -s /musl/etc/ssl /etc/ssl
fi
echo "LAFOCUS:github-clone:begin"
$BB timeout 180 $G clone https://github.com/oscomp/xv6-riscv.git /musl/xv6-riscv
clone_rc=$?
echo "LAFOCUS:github-clone:rc:$clone_rc"
if [ "$clone_rc" -eq 0 ]; then
    echo "LAFOCUS:github-clone:head:[$($G -C /musl/xv6-riscv rev-parse HEAD 2>&1)]"
    echo "LAFOCUS:github-clone:readme-bytes:[$($BB wc -c < /musl/xv6-riscv/README)]"
fi
echo "LAFOCUS:END"
GUESTEOF
    ;;
  github-shallow-clone)
    cat > "$WORK/tx-run.sh" <<'GUESTEOF'
BB=/musl/bin/busybox
export GIT_PAGER=cat HOME=/musl/root GIT_EXEC_PATH=/musl/usr/libexec/git-core \
       GIT_TEMPLATE_DIR= PATH=/musl/usr/bin:/musl/bin:/usr/bin:/bin
G="git -c gc.auto=0 -c maintenance.auto=false -c http.sslVerify=true -c http.proxy=http://10.0.2.2:7897"
$BB mkdir -p /etc 2>/dev/null
echo "nameserver 10.0.2.3" > /etc/resolv.conf
if [ ! -e /etc/ssl ]; then
    $BB ln -s /musl/etc/ssl /etc/ssl
fi
echo "LAFOCUS:github-shallow-clone:begin"
$BB timeout 180 $G clone --depth 1 https://github.com/oscomp/xv6-riscv.git /musl/xv6-riscv
clone_rc=$?
echo "LAFOCUS:github-shallow-clone:rc:$clone_rc"
if [ "$clone_rc" -eq 0 ]; then
    echo "LAFOCUS:github-shallow-clone:head:[$($G -C /musl/xv6-riscv rev-parse HEAD 2>&1)]"
    echo "LAFOCUS:github-shallow-clone:readme-bytes:[$($BB wc -c < /musl/xv6-riscv/README)]"
fi
echo "LAFOCUS:END"
GUESTEOF
    ;;
  connect-timeout)
    cat > "$WORK/tx-run.sh" <<'GUESTEOF'
BB=/musl/bin/busybox
export GIT_PAGER=cat HOME=/musl/root GIT_EXEC_PATH=/musl/usr/libexec/git-core \
       GIT_TEMPLATE_DIR= GIT_SSL_NO_VERIFY=true GIT_CURL_VERBOSE=1 \
       GIT_TRACE=1 GIT_TRACE_CURL=1 \
       PATH=/musl/usr/bin:/musl/bin:/usr/bin:/bin
G="git -c gc.auto=0 -c maintenance.auto=false -c http.sslVerify=false"
echo "LAFOCUS:connect-timeout:begin"
$BB timeout 8 $G ls-remote https://192.0.2.1:443/x.git
echo "LAFOCUS:connect-timeout:rc:$?"
echo "LAFOCUS:END"
GUESTEOF
    ;;
  *)
    echo "usage: $0 [clock|timer|dns|resolver|git-dns|git-dns-trace|github-ls-remote|github-clone|github-shallow-clone|connect-timeout]"
    exit 2
    ;;
esac

cp -f "$IMG" "$WORK/disk.img" \
  || { echo "FATAL: cannot create temporary guest disk: $WORK/disk.img"; exit 2; }
debugfs -w -R "write $WORK/tx-run.sh /tx-run.sh" "$WORK/disk.img" >/dev/null 2>&1

echo "arch: $ARCH"
echo "mode: $MODE"
echo "outer timeout: ${QEMU_TIMEOUT}s"
if [ "$MONITOR_ENABLED" = "1" ] && [ "$ARCH" = "la64" ]; then
  (
    sleep 12
    printf 'xp /2wx 0x10000020\nxp /1bx 0x10000112\nxp /1bx 0x10000212\nxp /2wx 0x100003a0\ninfo registers\n' \
      | timeout 3 nc -U "$MONITOR_SOCKET" > "$MONITOR_LOG" 2>&1
  ) &
fi
if [ "$ARCH" = "la64" ]; then
  timeout "$QEMU_TIMEOUT" qemu-system-loongarch64 \
    -machine virt -cpu la464 -kernel "$K" -m 1152M -nographic -smp 1 \
    -drive "file=$WORK/disk.img,if=none,format=raw,id=x0,file.locking=off" \
    -device virtio-blk-pci-non-transitional,drive=x0,addr=1 \
    -device virtio-net-pci,netdev=net,addr=2 -netdev user,id=net \
    "${MONITOR_ARGS[@]}" \
    "${TRACE_ARGS[@]}" \
    -no-reboot -rtc base=utc \
    -fw_cfg "name=opt/tx.cmdline,string=tx.runsh=/musl/tx-run.sh console=ttyS0" \
    -append "tx.runsh=/musl/tx-run.sh console=ttyS0" > "$SERIAL" 2>&1
else
  timeout "$QEMU_TIMEOUT" qemu-system-riscv64 \
    -machine virt -kernel "$K" -m 1G -nographic -smp 1 -bios default \
    -global virtio-mmio.force-legacy=false \
    -drive "file=$WORK/disk.img,if=none,format=raw,id=x0,file.locking=off" \
    -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
    -device virtio-net-device,netdev=net,bus=virtio-mmio-bus.1 \
    "${MONITOR_ARGS[@]}" \
    "${TRACE_ARGS[@]}" \
    -netdev user,id=net -no-reboot -rtc base=utc \
    -append "tx.runsh=/musl/tx-run.sh console=ttyS0" > "$SERIAL" 2>&1
fi
qemu_rc=$?

echo "qemu rc: $qemu_rc"
grep -a '^LAFOCUS:' "$SERIAL" || true
echo "raw guest serial: $SERIAL"
if [ "$MONITOR_ENABLED" = "1" ] && [ "$ARCH" = "la64" ]; then
  echo "raw qemu monitor: $MONITOR_LOG"
fi

grep -aq '^LAFOCUS:END' "$SERIAL"
