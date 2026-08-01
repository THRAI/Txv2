#!/usr/bin/env bash
# ltp-reconcile.sh — set-diff the LTP verdicts of two kernels, per module.
#
# `<` lines are baseline-only (a verdict we LOST = candidate regression),
# `>` lines are HEAD-only (a verdict we GAINED, or a changed message).
set -u
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIR="$ROOT/target/oscomp/ltp-runtest"
V="$ROOT/.v6work/ltp-verdicts.sh"
LANE="${LANE:-rv.musl}"
rc=0
for m in "$@"; do
  b="$DIR/base-$m-$LANE.log"; h="$DIR/head-$m-$LANE.log"
  echo "================ $m ($LANE) ================"
  if [ ! -f "$b" ] || [ ! -f "$h" ]; then
    echo "  MISSING log (base=$([ -f "$b" ] && echo ok || echo no) head=$([ -f "$h" ] && echo ok || echo no))"
    rc=1; continue
  fi
  bn=$(bash "$V" "$b" | wc -l); hn=$(bash "$V" "$h" | wc -l)
  bp=$(bash "$V" "$b" | grep -c 'TPASS'); hp=$(bash "$V" "$h" | grep -c 'TPASS')
  bf=$(bash "$V" "$b" | grep -cE 'TFAIL|TBROK'); hf=$(bash "$V" "$h" | grep -cE 'TFAIL|TBROK')
  echo "  verdicts base=$bn head=$hn | TPASS base=$bp head=$hp | TFAIL+TBROK base=$bf head=$hf"
  if diff <(bash "$V" "$b") <(bash "$V" "$h") > /dev/null; then
    echo "  IDENTICAL verdict set"
  else
    echo "  --- differences (< baseline-only = lost, > head-only = gained) ---"
    diff <(bash "$V" "$b") <(bash "$V" "$h") | sed 's/^/  /'
    rc=1
  fi
done
exit "$rc"
