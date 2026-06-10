#!/bin/bash
# Colored variant: forces LTP_COLORIZE_OUTPUT=y so logs carry the exact ANSI
# bytes the official judge_ltp-glibc.py matches. Also bind-mounts a readable
# /etc/hosts (AppArmor blocks the real one under userns).
set -u
TEST="$1"
OUT="$2"
LTPROOT=/tmp/ltp-install
BIN="$LTPROOT/testcases/bin"
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

WORK=$(mktemp -d /tmp/ltp-net-sweep/workc.XXXXXX)
export WORK BIN TEST
start=$(date +%s.%N)
unshare -r -n -m --propagation private bash -c "$(declare -f inner); inner" \
  >"$OUT/$TEST.log" 2>&1
end=$(date +%s.%N)
echo "$TEST $(echo "$end $start" | awk '{printf "%.1f", $1-$2}')" >> "$OUT/durations.txt"
rm -rf "$WORK"
