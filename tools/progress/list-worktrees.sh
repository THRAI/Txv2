#!/usr/bin/env sh
set -eu

if ! command -v jq >/dev/null 2>&1; then
  echo "error: jq is required" >&2
  exit 1
fi

for file in docs/progress/worktrees/*.json; do
  [ -e "$file" ] || exit 0
  jq -r --arg file "$file" '[.id, .status, .branch, .path, .owner, $file] | @tsv' "$file"
done
