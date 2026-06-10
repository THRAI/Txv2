#!/bin/bash
# Root variant of the LTP sweep runner. Same guardrails as run-one-full.sh
# (netns+mountns+pidns isolation, systemd cgroup cage, disk TMPDIR, 330s cap,
# colored output) but running with real root so loop devices, global
# sysctl/cgroup writes and setuid-to-nobody work. Only ever invoked on the
# pre-reviewed list (deny-listed host-clock/swap/hotplug tests excluded).
#
# ⚠️ INCIDENT 2026-06-10: this runner CRASHED the host (black screen + reboot).
# Root cause: pty03 (mkiss/N_AX25 tty line-discipline race via tst_fuzzy_sync)
# hung host kernel 6.14. tty LINE DISCIPLINES ARE NOT NAMESPACED — netns/pidns/
# mountns do NOT contain them; real root supplies CAP_NET_ADMIN which unlocks
# the N_AX25 ldisc (non-root runs got EPERM and were safe). NEVER run the
# pty03/pty04/pty06/pty07 ldisc-race family (or any tst_fuzzy_sync ldisc test)
# as root on the host. Run those, if ever, only inside a throwaway VM.
DENY_LDISC="pty03 pty04 pty06 pty07"
case " $DENY_LDISC " in *" $1 "*) echo "REFUSED: $1 is a host-crashing ldisc race test"; exit 0;; esac
set -u
TEST="$1"
OUT="$2"
LTPROOT=/tmp/ltp-install
BIN="$LTPROOT/testcases/bin"
SWEEP=/home/msp/learning/Txv2/target/ltp-full-sweep
mkdir -p "$OUT"

inner() {
  ip link set lo up 2>/dev/null
  ip link set lo multicast on 2>/dev/null
  ip route add 224.0.0.0/4 dev lo 2>/dev/null
  mount -t tmpfs tmpfs /run 2>/dev/null
  mkdir -p /run/netns 2>/dev/null
  export LTPROOT=/tmp/ltp-install
  export PATH="$BIN:/tmp/ltp-net-sweep/shims:$PATH"
  export TMPDIR="$WORK"
  export LTP_COLORIZE_OUTPUT=y
  cd "$WORK" || exit 99
  echo "RUN LTP CASE $TEST"
  timeout -k 5 330 "$BIN/$TEST"
  ret=$?
  echo "FAIL LTP CASE $TEST : $ret"
}

WORK=$(mktemp -d "$SWEEP/work/r.XXXXXX")
export WORK BIN TEST
start=$(date +%s.%N)
systemd-run --scope -q -p MemoryMax=2G -p TasksMax=1024 \
  unshare -n -m -p --fork --mount-proc --propagation private \
  bash -c "$(declare -f inner); inner" \
  >"$OUT/$TEST.log" 2>&1
end=$(date +%s.%N)
echo "$TEST $(echo "$end $start" | awk '{printf "%.1f", $1-$2}')" >> "$OUT/durations.txt"
rm -rf "$WORK"
