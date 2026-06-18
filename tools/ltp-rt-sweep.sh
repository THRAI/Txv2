#!/bin/bash
# Same-口径 ltp-runtest sweep for main-vs-merged LTP parity.
# Runs `ltp-runtest:syscalls:<cases>` in chunks on one lane against a chosen
# kernel (KERNEL_DIR), using THIS tree's sdcard (userspace held constant, so
# only the kernel differs). Accumulates per-case "case pass total" scores.
# Args: <kernel_dir> <lane> <tag>
set -u
cd /home/msp/learning/Txv2 || exit 1
KDIR="$1"; LANE="$2"; TAG="$3"
CASES=/tmp/la-whitelist-cases.txt
OUTDIR=target/oscomp/ltp-runtest
OUT="$OUTDIR/$TAG.scores"; PROG="$OUTDIR/$TAG.prog"
mkdir -p "$OUTDIR"; : > "$OUT"; : > "$PROG"
mapfile -t ALL < <(grep -vx 'case' "$CASES" | grep -E '^[a-z0-9_]+$')
CHUNK=60; i=0; n=${#ALL[@]}
echo "[$(date +%H:%M:%S)] $TAG start: $n cases, kernel=$KDIR lane=$LANE" >> "$PROG"
while [ $i -lt $n ]; do
  chunk=("${ALL[@]:$i:$CHUNK}")
  files=$(IFS=+; echo "${chunk[*]}")
  tag="rtsw-$TAG-$i"
  KERNEL_DIR="$KDIR" MODULE=syscalls tools/ltp-runtest-witness.sh 620 "$LANE" "$files" "$tag" >/dev/null 2>&1
  grep -aE '^[[:space:]]*[✓~✗?][[:space:]]' "$OUTDIR/$tag-$LANE.judge" 2>/dev/null \
    | sed -E 's/^[[:space:]]*[✓~✗?][[:space:]]+([a-z0-9_]+)[[:space:]]+([0-9]+)\/([0-9]+).*/\1 \2 \3/' \
    | grep -E '^[a-z0-9_]+ [0-9]+ [0-9]+$' >> "$OUT"
  # reclaim the 4 GiB per-run sdcard copy immediately (disk hygiene)
  : > "$OUTDIR/sd-$tag-$LANE.img" 2>/dev/null
  i=$((i+CHUNK))
  echo "[$(date +%H:%M:%S)] $TAG chunk@$i/$n (scored=$(wc -l < "$OUT"))" >> "$PROG"
done
echo "[$(date +%H:%M:%S)] $TAG DONE (scored=$(wc -l < "$OUT"))" >> "$PROG"
