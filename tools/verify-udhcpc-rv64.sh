#!/usr/bin/env bash
# Compatibility entry point for the explicit fixed-topology DHCP fixture.
#
# VALIDATION-SCOPE WARNING:
# The optional Git probe writes to `/home` tmpfs while the ext4 image is a
# `/musl` sidecar.  A pass validates DHCP/DNS/TLS/Git transport only; it is
# invalid evidence for direct-root ext4 RW, JBD2, or reboot persistence.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
exec bash "$ROOT/tools/network-scenarios/fixtures/legacy/verify-udhcpc-rv64.sh" "$@"
