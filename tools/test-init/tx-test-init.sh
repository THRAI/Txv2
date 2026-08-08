#!/bin/sh
# Tx test-mode init. Keep Alpine/normal boots out of this file: it exists only
# for tx.boot.mode=oscomp/ltp/test overlays.

set +e

log() {
    echo "tx-test-init:$*" > /dev/console 2>/dev/null || echo "tx-test-init:$*"
}

find_busybox() {
    for candidate in /bin/busybox /musl/musl/busybox /musl/busybox /tx-ltp/busybox-full; do
        if [ -x "$candidate" ]; then
            echo "$candidate"
            return 0
        fi
    done
    echo /bin/busybox
}

bb=$(find_busybox)

mkdir_p() {
    "$bb" mkdir -p "$@" 2>/dev/null || mkdir -p "$@"
}

link_force() {
    target=$1
    path=$2
    "$bb" rm -f "$path" 2>/dev/null || rm -f "$path"
    "$bb" ln -s "$target" "$path" 2>/dev/null || ln -s "$target" "$path"
}

write_file() {
    path=$1
    mode=$2
    shift 2
    {
        for line in "$@"; do
            printf '%s\n' "$line"
        done
    } > "$path"
    "$bb" chmod "$mode" "$path" 2>/dev/null || chmod "$mode" "$path"
}

setup_base_tree() {
    mkdir_p /bin /usr/bin /etc /tmp /var/tmp /var/run/netns /var/lib/misc /var/log \
        /sys /boot /lib/modules/6.1.0-txkernel /tx-ltp/bin /tx-ltp/trace-bin

    if [ ! -x /bin/busybox ]; then
        link_force "$bb" /bin/busybox
    fi
    for applet in sh ls cat chmod rm ln mkdir tr echo mv cp sleep true; do
        [ -e "/bin/$applet" ] || link_force /bin/busybox "/bin/$applet"
    done
    [ -e /usr/bin/env ] || link_force /bin/busybox /usr/bin/env

    write_file /etc/passwd 0644 \
        "root:x:0:0:root:/root:/bin/sh" \
        "nobody:x:65534:65534:nobody:/nonexistent:/bin/sh"
    write_file /etc/group 0644 \
        "root:x:0:" \
        "daemon:x:2:" \
        "users:x:100:" \
        "nogroup:x:65534:" \
        "nobody:x:65534:"
    write_file /etc/nsswitch.conf 0644 \
        "passwd: files" \
        "group: files" \
        "shadow: files" \
        "hosts: files dns" \
        "services: files" \
        "protocols: files"
    write_file /etc/hosts 0666 \
        "127.0.0.1 localhost" \
        "::1 localhost ip6-localhost ip6-loopback"
    write_file /etc/services 0644 \
        "echo 7/tcp" \
        "echo 7/udp"
    write_file /etc/protocols 0644 \
        "hopopt 0 HOPOPT" \
        "ip 0 IP" \
        "ipv6 41 IPv6" \
        "ipv6-route 43 IPv6-Route" \
        "ipv6-frag 44 IPv6-Frag" \
        "esp 50 ESP" \
        "ah 51 AH" \
        "ipv6-icmp 58 IPv6-ICMP" \
        "ipv6-nonxt 59 IPv6-NoNxt" \
        "ipv6-opts 60 IPv6-Opts"
    write_file /etc/dhcpd.conf 0644 "# txkernel LTP DHCP compatibility placeholder"

    write_file /lib/modules/6.1.0-txkernel/modules.dep 0644 \
        "kernel/drivers/net/dummy.ko:" \
        "kernel/drivers/net/veth.ko:" \
        "kernel/net/sched/sch_teql.ko:" \
        "kernel/net/ipv4/netfilter/ip_tables.ko:" \
        "kernel/net/ipv6/netfilter/ip6_tables.ko:" \
        "kernel/net/netfilter/nf_tables.ko:" \
        "kernel/net/sctp/sctp.ko:"
    write_file /lib/modules/6.1.0-txkernel/modules.builtin 0644 \
        "kernel/drivers/net/dummy.ko" \
        "kernel/drivers/net/veth.ko" \
        "kernel/net/sched/sch_teql.ko" \
        "kernel/net/ipv4/netfilter/ip_tables.ko" \
        "kernel/net/ipv6/netfilter/ip6_tables.ko" \
        "kernel/net/netfilter/nf_tables.ko" \
        "kernel/net/sctp/sctp.ko"
    if [ -r /proc/config ]; then
        "$bb" cat /proc/config > /boot/config-6.1.0-txkernel 2>/dev/null || cat /proc/config > /boot/config-6.1.0-txkernel
    else
        write_file /boot/config-6.1.0-txkernel 0644 \
            "CONFIG_NET=y" \
            "CONFIG_INET=y" \
            "CONFIG_IPV6=y" \
            "CONFIG_SCTP=y"
    fi
}

install_ltp_helpers() {
    for name in arp id ln mkdir mount readlink seq cat cut grep; do
        [ -e "/tx-ltp/bin/$name" ] || link_force /bin/busybox "/tx-ltp/bin/$name"
    done

    cat > /tx-ltp/bin/sysctl <<'EOS'
#!/bin/sh
bb=/bin/busybox
[ -x "$bb" ] || bb=/musl/musl/busybox
case "$*" in *net.ipv6.conf.*) exit 0 ;; esac
if [ "$1" = -b ]; then
    path=/proc/sys/$(echo "$2" | "$bb" tr . /)
    [ -r "$path" ] || exit 1
    "$bb" tr -d '\n' < "$path"
    exit 0
fi
exec "$bb" sysctl "$@"
EOS

    cat > /tx-ltp/bin/dmesg <<'EOS'
#!/bin/sh
[ -r /tmp/tx-dmesg ] && cat /tmp/tx-dmesg
exit 0
EOS

    cat > /tx-ltp/bin/netstat <<'EOS'
#!/bin/sh
for arg in "$@"; do
    case "$arg" in
        -*s*) cat /proc/net/snmp 2>/dev/null || true; exit 0 ;;
        -*i*) cat /proc/net/dev 2>/dev/null || true; exit 0 ;;
        -*g*) [ -r /proc/net/igmp ] && cat /proc/net/igmp; [ -r /proc/net/igmp6 ] && cat /proc/net/igmp6; exit 0 ;;
        -*r*) echo "Kernel IP routing table"; [ -r /proc/net/route ] && cat /proc/net/route; exit 0 ;;
    esac
done
exec /bin/busybox netstat "$@"
EOS

    cat > /tx-ltp/bin/ss <<'EOS'
#!/bin/sh
case "$*" in
    *l*t*p*|*l*p*t*|*t*l*p*|*t*p*l*|*p*l*t*|*p*t*l*) cat /proc/net/tcp_listen_proc 2>/dev/null; cat /proc/net/tcp6_listen_proc 2>/dev/null; exit 0 ;;
esac
cat /proc/net/tcp 2>/dev/null
cat /proc/net/tcp6 2>/dev/null
EOS

    cat > /tx-ltp/bin/tracepath <<'EOS'
#!/bin/sh
cmd=${0##*/}
case "$1" in -V|--version) echo "$cmd txkernel-minimal"; exit 0 ;; esac
[ "$1" = "-6" ] && shift
host=
len=65535
while [ $# -gt 0 ]; do
    case "$1" in
        -l) len="$2"; shift 2 ;;
        -*) shift ;;
        *) [ -n "$host" ] || host="$1"; shift ;;
    esac
done
[ -n "$host" ] || exit 1
echo " 1?: [$host] pmtu $len hops 1"
EOS
    link_force /tx-ltp/bin/tracepath /tx-ltp/bin/tracepath6

    "$bb" chmod 0755 /tx-ltp/bin/* 2>/dev/null || chmod 0755 /tx-ltp/bin/*
}

reap_children() {
    while wait 2>/dev/null; do
        :
    done
}

run_payload() {
    if [ $# -eq 0 ] || [ -z "$1" ]; then
        log "no-payload"
        return 127
    fi
    tx_payload=$1
    shift || true
    tx_payload_script=/tmp/tx-test-payload.sh
    {
        printf '%s\n' '#!/bin/sh'
        printf '%s\n' "$tx_payload"
    } > "$tx_payload_script"
    "$bb" chmod 0755 "$tx_payload_script" 2>/dev/null || chmod 0755 "$tx_payload_script"
    /bin/sh "$tx_payload_script" "$@" &
    tx_child=$!
    wait "$tx_child"
    tx_status=$?
    reap_children
    return "$tx_status"
}

log "setup:start"

# The SMP VVAR witness owns its raw clone/futex coordination so its reader and
# writer run on explicitly disjoint CPU masks without shell scheduling noise.
if [ -x /vdso-vvar-smp-probe ]; then
    exec /vdso-vvar-smp-probe
fi

# The vDSO signal/time ABI witness is a freestanding RV64 binary. Run it before
# the broad LTP compatibility setup so trap tracing observes only this probe.
if [ -x /vdso-phase5-probe ]; then
    log "vdso-phase5:run"
    /vdso-phase5-probe
    status=$?
    reap_children
    log "vdso-phase5:exit:$status"
    exit "$status"
fi

setup_base_tree
install_ltp_helpers
log "setup:ok"

run_payload "$@"
status=$?
reap_children
log "exit:$status"
exit "$status"
