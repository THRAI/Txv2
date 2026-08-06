#!/bin/sh
# Guest-side witness for tools/verify-udhcpc-rv64.sh.

BB=/bin/busybox
IP=/sbin/ip
[ -x "$IP" ] || IP=/usr/sbin/ip

iface_selector=""
for cmdline_token in $($BB cat /proc/cmdline 2>/dev/null); do
    case "$cmdline_token" in
        tx.net.iface=*) iface_selector="${cmdline_token#tx.net.iface=}" ;;
    esac
done

echo "TXDHCP:begin"

echo "TXDHCP:link-inventory"
$IP link show 2>&1

iface=""
iface_count=0
for net_path in /sys/class/net/*; do
    [ -e "$net_path" ] || continue
    candidate="${net_path##*/}"
    if [ "$candidate" != "lo" ]; then
        iface="$candidate"
        iface_count=$((iface_count + 1))
    fi
done
if [ -n "$iface_selector" ]; then
    iface="$iface_selector"
elif [ "$iface_count" -gt 1 ]; then
    echo "TXDHCP:error:ambiguous-interface"
    echo "TXDHCP:end"
    $BB poweroff -f
    exit 1
fi
if [ -z "$iface" ]; then
    iface="$($IP link show 2>/dev/null | $BB awk -F': ' '/^[0-9]+: / && $2 != "lo" { split($2, n, "@"); print n[1]; exit }')"
fi
if [ -z "$iface" ] || [ ! -e "/sys/class/net/$iface" ]; then
    echo "TXDHCP:error:no-non-loopback-interface"
    echo "TXDHCP:end"
    $BB poweroff -f
    exit 1
fi

echo "TXDHCP:interface:$iface"
echo "TXDHCP:before"
$IP -4 addr show dev "$iface"
$IP -4 route show

$IP link set dev "$iface" up
/sbin/udhcpc -f -q -n -t 3 -T 3 -i "$iface" \
    -s /usr/share/udhcpc/default.script
udhcpc_rc=$?

lease="$($IP -4 addr show dev "$iface" 2>/dev/null | $BB awk '/inet / { split($2, n, "/"); print n[1]; exit }')"
gateway="$($IP -4 route show default 2>/dev/null | $BB awk '/^default / { print $3; exit }')"

echo "TXDHCP:udhcpc-rc:$udhcpc_rc"
echo "TXDHCP:lease:$lease"
echo "TXDHCP:gateway:$gateway"
echo "TXDHCP:after"
$IP -4 addr show dev "$iface"
$IP -4 route show
$BB cat /etc/resolv.conf 2>/dev/null

if [ "$udhcpc_rc" -eq 0 ] && [ -n "$lease" ]; then
    echo "TXDHCP:lease-result:pass"
else
    echo "TXDHCP:lease-result:fail"
fi

if [ -n "$gateway" ] && $BB timeout 10 $BB ping -c 1 "$gateway" >/tmp/tx-dhcp-ping.log 2>&1; then
    echo "TXDHCP:gateway-result:pass"
else
    echo "TXDHCP:gateway-result:fail"
    $BB tail -n 8 /tmp/tx-dhcp-ping.log 2>/dev/null
fi

dns_server="$($BB awk '/^nameserver / { print $2; exit }' /etc/resolv.conf 2>/dev/null)"
if [ -n "$dns_server" ] \
    && $BB timeout 20 $BB nslookup github.com "$dns_server" >/tmp/tx-dhcp-dns.log 2>&1; then
    echo "TXDHCP:dns-result:pass"
else
    echo "TXDHCP:dns-result:fail"
    $BB tail -n 8 /tmp/tx-dhcp-dns.log 2>/dev/null
fi

git_remote=""
git_trace="0"
for cmdline_token in $($BB cat /proc/cmdline 2>/dev/null); do
    case "$cmdline_token" in
        tx.git.remote=*) git_remote="${cmdline_token#tx.git.remote=}" ;;
        tx.git.trace=1) git_trace="1" ;;
    esac
done
if [ -n "$git_remote" ]; then
    echo "TXDHCP:git-prepare"
    clone_dir="/musl/root/tx-dhcp-clone-$$"
    ca_bundle=""
    for cert_root in /etc/ssl /musl/etc/ssl; do
        [ -d "$cert_root" ] || continue
        for cert_candidate in $($BB find "$cert_root" -type f 2>/dev/null); do
            if $BB grep -q 'BEGIN CERTIFICATE' "$cert_candidate" 2>/dev/null; then
                ca_bundle="$cert_candidate"
                break
            fi
        done
        [ -n "$ca_bundle" ] && break
    done
    if [ -n "$ca_bundle" ]; then
        export GIT_SSL_CAINFO="$ca_bundle"
    fi
    if [ "$git_trace" = "1" ]; then
        export GIT_TRACE=/dev/console
        export GIT_TRACE_CURL=/dev/console
    fi
    echo "TXDHCP:git-ca:$ca_bundle"
    echo "TXDHCP:git-start"
    if $BB timeout -k 5 90 /usr/bin/git -c http.version=HTTP/1.1 \
        clone --depth=1 "$git_remote" "$clone_dir" \
        >/tmp/tx-dhcp-git.log 2>&1; then
        echo "TXDHCP:git-result:pass"
        echo "TXDHCP:git-head:$(/usr/bin/git -C "$clone_dir" rev-parse --short HEAD 2>/dev/null)"
    else
        echo "TXDHCP:git-result:fail"
        $BB tail -n 12 /tmp/tx-dhcp-git.log 2>/dev/null
    fi
fi

echo "TXDHCP:end"
$BB poweroff -f
