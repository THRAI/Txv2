#!/bin/bash
# One-shot root re-measurement of the 375 sandbox-blocked LTP files.
# Launches the batch in the background and returns immediately; progress in
# results-root/durations.txt. No persistent privileges are kept afterwards.
set -eu
SWEEP=/home/msp/learning/Txv2/target/ltp-full-sweep
LIST=/tmp/root-rerun-list.txt

[ "$(id -u)" = 0 ] || { echo "must run with sudo"; exit 1; }
[ -f "$LIST" ] || { echo "missing $LIST"; exit 1; }
chmod +x "$SWEEP/run-one-root.sh"
mkdir -p "$SWEEP/results-root" "$SWEEP/work"

setsid nohup bash -c "
  cd $SWEEP
  xargs -P 8 -I{} ./run-one-root.sh {} $SWEEP/results-root < $LIST
  echo ROOT_SWEEP_DONE >> $SWEEP/results-root/durations.txt
  chown -R msp:msp $SWEEP/results-root
" >/dev/null 2>&1 &

echo "started: $(wc -l < $LIST) tests, 8-way parallel, ~20-40 min"
echo "watch:   wc -l $SWEEP/results-root/durations.txt"
