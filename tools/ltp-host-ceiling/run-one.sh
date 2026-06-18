#!/bin/bash
# Run one LTP test file no-arg, the official OSComp way, in an isolated sandbox.
# Usage: run-one.sh <testname> <outdir>
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
  export LTPROOT=/tmp/ltp-install
  export PATH="$BIN:/tmp/ltp-net-sweep/shims:$PATH"
  export TMPDIR="$WORK"
  cd "$WORK" || exit 99
  echo "RUN LTP CASE $TEST"
  timeout -k 5 330 "$BIN/$TEST"
  ret=$?
  echo "FAIL LTP CASE $TEST : $ret"
}

WORK=$(mktemp -d /tmp/ltp-net-sweep/work.XXXXXX)
export WORK BIN TEST
start=$(date +%s.%N)
unshare -r -n -m --propagation private bash -c "$(declare -f inner); inner" \
  >"$OUT/$TEST.log" 2>&1
end=$(date +%s.%N)
echo "$TEST $(echo "$end $start" | awk '{printf "%.1f", $1-$2}')" >> "$OUT/durations.txt"
rm -rf "$WORK"
