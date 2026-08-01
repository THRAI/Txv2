#!/usr/bin/env bash
# tx-shell.sh — boot txKernel into an INTERACTIVE shell with git + network
# ready, so you can type `git clone` / `git push` / `git pull` live at a `~ #`
# prompt WITHOUT ever typing your token over the (fragile) serial console.
#
# Usage:
#   bash tools/tx-shell.sh                              # rv64, read-only (public repos)
#   TX_GH_TOKEN=github_pat_xxx bash tools/tx-shell.sh   # rv64, can push to your repos
#   TX_GH_TOKEN=github_pat_xxx bash tools/tx-shell.sh la64
#
# The token is passed as an env var on YOUR terminal (paste works fine there),
# baked into a GIT_ASKPASS helper inside the guest, so git authenticates
# automatically. Inside the `~ #` prompt you type PLAIN urls, no token:
#   git clone https://github.com/LLLPPPS/tx-push-test.git
#   cd tx-push-test
#   echo hi > /tmp/n; cp /tmp/n note.txt; git add .; git commit -m x; git push
#   git clone https://github.com/oscomp/xv6-riscv.git      # read-only, no token needed
#
# Quit: type `exit`, or press Ctrl-A then X.
#
# Notes:
# - Boots a THROWAWAY copy of the image; your changes never touch the base image.
# - This sandbox reaches github only via a Clash proxy (exported automatically);
#   on a direct-internet network run with TX_PROXY= to disable it.
# - Create files to commit with `echo ... > /tmp/x; cp /tmp/x target` — a plain
#   `echo > ext4file` can land empty (known cross-process ext4 write quirk).
# - Needs the kernel built: cargo xtask build --target {rv64,la64}-qemu
set -u
ARCH="${1:-rv64}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROXY="${TX_PROXY-http://10.0.2.2:7897}"
TOKEN="${TX_GH_TOKEN-}"
WORK="$(mktemp -d /tmp/txshell-XXXXXX)"

case "$ARCH" in
  rv64)
    K="$ROOT/target/riscv64gc-unknown-none-elf/debug/tx-kernel-riscv64-qemu-virt"
    IMG="$ROOT/local-images/alpine-linux-riscv64-ext4fs.img" ;;
  la64)
    K="$ROOT/target/loongarch64-unknown-none-softfloat/debug/tx-kernel-loongarch64-qemu-virt"
    IMG="$ROOT/local-images/alpine-linux-loongarch64-ext4fs.img" ;;
  *) echo "usage: tx-shell.sh [rv64|la64]"; exit 2 ;;
esac
[ -f "$K" ]   || { echo "kernel missing: $K"; echo "build it: cargo xtask build --target ${ARCH}-qemu"; exit 2; }
[ -f "$IMG" ] || { echo "image missing: $IMG"; exit 2; }

# Build the guest bootstrap. The GIT_ASKPASS helper carries the token so it is
# never typed at the prompt. Written to /tmp (tmpfs — safe cross-process).
{
  cat <<G
export PATH=/musl/usr/bin:/musl/bin:/musl/usr/sbin:/musl/sbin:/usr/bin:/bin
export HOME=/musl/root GIT_EXEC_PATH=/musl/usr/libexec/git-core GIT_TEMPLATE_DIR= GIT_SSL_NO_VERIFY=true
export HTTPS_PROXY=$PROXY HTTP_PROXY=$PROXY ALL_PROXY=$PROXY
export GIT_AUTHOR_NAME=txkernel GIT_AUTHOR_EMAIL=tx@txkernel.local
export GIT_COMMITTER_NAME=txkernel GIT_COMMITTER_EMAIL=tx@txkernel.local
export GIT_TERMINAL_PROMPT=0
/bin/busybox mkdir -p /etc /musl/root
echo "nameserver 10.0.2.3" > /etc/resolv.conf
G
  if [ -n "$TOKEN" ]; then
    cat <<G
/bin/busybox printf '#!/bin/sh\ncase "\$1" in *[Uu]sername*) echo x-access-token;; *) echo %s;; esac\n' '$TOKEN' > /tmp/tx-askpass
/bin/busybox chmod 755 /tmp/tx-askpass
export GIT_ASKPASS=/tmp/tx-askpass
G
  fi
  cat <<'G'
cd /musl/root
/bin/busybox echo ""
/bin/busybox echo "=== txKernel interactive shell — git ready (auth auto). Try:"
/bin/busybox echo "===   git clone https://github.com/LLLPPPS/tx-push-test.git"
/bin/busybox echo "===   git clone https://github.com/oscomp/xv6-riscv.git   (read-only)"
/bin/busybox echo "=== quit: exit  (or Ctrl-A X)"
exec /bin/busybox sh -i
G
} > "$WORK/tx-run.sh"

cp -f "$IMG" "$WORK/disk.img"
debugfs -w -R "write $WORK/tx-run.sh /tx-run.sh" "$WORK/disk.img" >/dev/null 2>&1
[ -n "$TOKEN" ] && echo "token: loaded (git will auth automatically; type PLAIN github urls)" \
                || echo "token: none (read-only clones only; set TX_GH_TOKEN=... to push)"
echo "booting txKernel ($ARCH) interactive shell — a '~ #' prompt appears after ~15-30s..."

if [ "$ARCH" = rv64 ]; then
  exec qemu-system-riscv64 -machine virt -kernel "$K" -m 1G -nographic -smp 1 -bios default \
    -global virtio-mmio.force-legacy=false \
    -drive "file=$WORK/disk.img,if=none,format=raw,id=x0,file.locking=off" \
    -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
    -device virtio-net-device,netdev=net,bus=virtio-mmio-bus.1 -netdev user,id=net \
    -no-reboot -rtc base=utc -append "tx.runsh=/musl/tx-run.sh console=ttyS0"
else
  exec qemu-system-loongarch64 -machine virt -cpu la464 -kernel "$K" -m 1152M -nographic -smp 1 \
    -drive "file=$WORK/disk.img,if=none,format=raw,id=x0,file.locking=off" \
    -device virtio-blk-pci-non-transitional,drive=x0,addr=1 \
    -device virtio-net-pci,netdev=net,addr=2 -netdev user,id=net \
    -rtc base=utc \
    -fw_cfg "name=opt/tx.cmdline,string=tx.runsh=/musl/tx-run.sh console=ttyS0" \
    -append "tx.runsh=/musl/tx-run.sh console=ttyS0"
fi
