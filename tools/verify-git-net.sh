#!/usr/bin/env bash
# Compatibility entry point for the explicit fixed-topology regression fixture.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
exec bash "$ROOT/tools/network-scenarios/fixtures/legacy/verify-git-net-rv64.sh" "$@"
