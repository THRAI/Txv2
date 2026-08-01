#!/usr/bin/env bash
# ltp-verdicts.sh <log> — normalise an LTP runtest log into a comparable
# verdict set: one `<case> <VERDICT> <message>` line per distinct result,
# sorted and deduped. Strips ANSI colour and the per-run iteration number so
# two runs of the same kernel produce byte-identical output.
#
# The judge score is useless for this comparison (the official shell-test form
# collapses a whole module into a single 0/1 item); the TPASS/TFAIL/TBROK/TCONF
# lines are the real signal.
set -u
# Two output shapes must both match: the shell library prints
#   `case 1 TPASS: msg`
# while the C binaries pad the colon
#   `asapi_01    2  TFAIL  :  asapi_01.c:119: msg`
# Normalise both to `case VERDICT msg` with single spaces.
sed 's/\x1b\[[0-9;]*m//g' "${1:?log}" \
  | grep -aoE '^[a-zA-Z0-9_.-]+ +[0-9]+ +T(PASS|FAIL|BROK|CONF|WARN) *: *.*' \
  | sed -E 's/^([a-zA-Z0-9_.-]+) +[0-9]+ +(T[A-Z]+) *: */\1 \2 /' \
  | tr -s ' ' \
  | sort -u
