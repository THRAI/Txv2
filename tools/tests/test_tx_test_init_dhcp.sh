#!/bin/sh

set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
test_init="$repo_root/tools/test-init/tx-test-init.sh"

TX_TEST_INIT_LIB_ONLY=1
export TX_TEST_INIT_LIB_ONLY
. "$test_init"

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    exit 1
}

assert_eq() {
    [ "$1" = "$2" ] || fail "expected '$2', got '$1'"
}

test_tmp=$(mktemp -d /tmp/tx-test-init-dhcp-XXXXXX)
trap 'rm -rf "$test_tmp"' EXIT HUP INT TERM

TEST_LOG=
log() {
    TEST_LOG="${TEST_LOG}${TEST_LOG:+|}$*"
}

one_net="$test_tmp/net-one"
mkdir -p "$one_net/lo" "$one_net/enp0s2"
TX_TEST_NET_CLASS_DIR=$one_net
select_dhcp_interface "" || fail "single interface was rejected"
assert_eq "$dhcp_iface" enp0s2

lo_only_net="$test_tmp/net-lo-only"
mkdir -p "$lo_only_net/lo"
TX_TEST_NET_CLASS_DIR=$lo_only_net
TEST_LOG=
if select_dhcp_interface ""; then
    fail "loopback-only interface inventory was accepted"
fi
assert_eq "$TEST_LOG" dhcp:fail:no-interface

mkdir -p "$one_net/enp0s3"
TX_TEST_NET_CLASS_DIR=$one_net
TEST_LOG=
if select_dhcp_interface ""; then
    fail "ambiguous interface inventory was accepted"
fi
assert_eq "$dhcp_iface" ""
assert_eq "$TEST_LOG" dhcp:fail:ambiguous-interface

select_dhcp_interface enp0s3 || fail "explicit interface selector was rejected"
assert_eq "$dhcp_iface" enp0s3
TEST_LOG=
if select_dhcp_interface lo; then
    fail "loopback selector was accepted for DHCP"
fi
assert_eq "$TEST_LOG" dhcp:fail:invalid-interface:lo

bb=/bin/busybox
TX_TEST_DHCP_HOOK="$test_tmp/udhcpc.script"
write_udhcpc_hook || fail "failed to render udhcpc hook"
sh -n "$TX_TEST_DHCP_HOOK"
grep -F 'lease_mask=$mask' "$TX_TEST_DHCP_HOOK" >/dev/null \
    || fail "hook does not prefer the DHCP prefix length"
grep -F 'lease_address="$ip/$lease_mask"' "$TX_TEST_DHCP_HOOK" >/dev/null \
    || fail "hook does not install the lease address"
grep -F 'ip -4 route add default via "$gateway"' "$TX_TEST_DHCP_HOOK" >/dev/null \
    || fail "hook does not install a default route"
grep -F 'nameserver %s' "$TX_TEST_DHCP_HOOK" >/dev/null \
    || fail "hook does not write DNS servers"

mock_net="$test_tmp/net-mock"
mkdir -p "$mock_net/lo" "$mock_net/enp0s2"
TX_TEST_NET_CLASS_DIR=$mock_net
MOCK_LEASE_READY=0
MOCK_UDHCPC_STATUS=0
mock_bb() {
    applet=$1
    shift
    case "$applet" in
        ip)
            if [ "${1:-}" = -4 ] && [ "${2:-}" = addr ] && [ "${3:-}" = show ]; then
                if [ "$MOCK_LEASE_READY" = 1 ]; then
                    printf '    inet 192.0.2.20/24 scope global enp0s2\n'
                fi
            elif [ "${1:-}" = -4 ] && [ "${2:-}" = route ] && [ "${3:-}" = show ]; then
                if [ "$MOCK_LEASE_READY" = 1 ]; then
                    printf 'default via 192.0.2.1 dev enp0s2\n'
                fi
            fi
            return 0
            ;;
        udhcpc)
            if [ "$MOCK_UDHCPC_STATUS" -eq 0 ]; then
                MOCK_LEASE_READY=1
            fi
            return "$MOCK_UDHCPC_STATUS"
            ;;
        awk)
            if [ "${2:-}" = /etc/resolv.conf ] && [ "$MOCK_LEASE_READY" = 1 ]; then
                printf '192.0.2.53\n'
            else
                command awk "$@"
            fi
            ;;
        *)
            return 0
            ;;
    esac
}
bb=mock_bb
write_udhcpc_hook() {
    dhcp_hook="$test_tmp/mock-udhcpc.script"
    return 0
}

TEST_LOG=
bootstrap_userspace_dhcp "" || fail "mock DHCP success was rejected"
assert_eq "$TEST_LOG" \
    'dhcp:start|dhcp:interface:enp0s2|dhcp:lease:192.0.2.20|dhcp:ok'

MOCK_LEASE_READY=0
MOCK_UDHCPC_STATUS=7
TEST_LOG=
if bootstrap_userspace_dhcp ""; then
    fail "mock udhcpc failure was accepted"
fi
assert_eq "$TEST_LOG" \
    'dhcp:start|dhcp:interface:enp0s2|dhcp:fail:udhcpc:7'

TEST_MODE=dhcp
TEST_IFACE=
TEST_BOOTSTRAP_STATUS=1
BOOTSTRAP_SELECTOR=
PAYLOAD_CALLS=0
cmdline_value() {
    case "$1" in
        tx.net.mode) printf '%s\n' "$TEST_MODE" ;;
        tx.net.iface) printf '%s\n' "$TEST_IFACE" ;;
        *) return 1 ;;
    esac
}
bootstrap_userspace_dhcp() {
    BOOTSTRAP_SELECTOR=$1
    return "$TEST_BOOTSTRAP_STATUS"
}
run_payload() {
    PAYLOAD_CALLS=$((PAYLOAD_CALLS + 1))
    return 0
}

if run_payload_with_network_bootstrap payload; then
    fail "payload gate accepted a failed DHCP bootstrap"
fi
assert_eq "$PAYLOAD_CALLS" 0

TEST_BOOTSTRAP_STATUS=0
TEST_IFACE=enp0s3
run_payload_with_network_bootstrap payload || fail "payload gate rejected successful DHCP"
assert_eq "$PAYLOAD_CALLS" 1
assert_eq "$BOOTSTRAP_SELECTOR" enp0s3

TEST_MODE=none
run_payload_with_network_bootstrap payload || fail "non-DHCP mode was rejected"
assert_eq "$PAYLOAD_CALLS" 2

printf 'PASS: tx-test-init DHCP host fixtures\n'
