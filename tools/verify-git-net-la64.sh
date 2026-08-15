#!/usr/bin/env bash
# Compatibility entry point for the explicit fixed-topology regression fixture.
#
# CRITICAL VALIDATION-SCOPE WARNING:
# This command is only a network/Git protocol witness.  The guest Git
# worktrees live on `/home` tmpfs and the ext4 image is only a `/musl`
# sidecar.  A passing result is invalid evidence for direct-root ext4 RW,
# JBD2 durability, clean unmount, or survival across a reboot.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
exec bash "$ROOT/tools/network-scenarios/fixtures/legacy/verify-git-net-la64.sh" "$@"
