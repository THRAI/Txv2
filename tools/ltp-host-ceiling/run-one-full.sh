#!/bin/bash
# Full-image LTP sweep runner: official no-arg form, colored output, caged.
# Cage: systemd user scope MemoryMax=2G TasksMax=1024 (protects host from
# oom/fork tests); userns+netns+mountns sandbox; disk-backed TMPDIR; 330s cap.
set -u
TEST="$1"
OUT="$2"
LTPROOT=/tmp/ltp-install
BIN="$LTPROOT/testcases/bin"
SWEEP=/home/msp/learning/Txv2/target/ltp-full-sweep
mkdir -p "$OUT"

inner() {
  ip link set lo up 2>/dev/null
  mount -t tmpfs tmpfs /run 2>/dev/null
  mkdir -p /run/netns 2>/dev/null
  mount --bind /tmp/hosts-shim /etc/hosts 2>/dev/null
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

WORK=$(mktemp -d "$SWEEP/work/w.XXXXXX")
export WORK BIN TEST
start=$(date +%s.%N)
systemd-run --user --scope -q -p MemoryMax=2G -p TasksMax=1024 \
  unshare -r -n -m -p --fork --mount-proc --propagation private bash -c "$(declare -f inner); inner" \
  >"$OUT/$TEST.log" 2>&1
end=$(date +%s.%N)
echo "$TEST $(echo "$end $start" | awk '{printf "%.1f", $1-$2}')" >> "$OUT/durations.txt"
rm -rf "$WORK"
