BB=/musl/bin/busybox
export PATH=/musl/sbin:/musl/usr/sbin:/musl/usr/bin:/musl/bin:/sbin:/usr/bin:/bin HOME=/musl/root
one() { $BB tr '\n' ';' ; }
say() { echo "V6:$1:[out=$($BB cat /tmp/$1.out 2>/dev/null | one)err=$($BB cat /tmp/$1.err 2>/dev/null | one)]"; }

echo "V6:begin:[v52-acceptance]"
# ** NO `ip -6` COMMANDS AT ALL ** — that is the whole point of V5-2.
echo "V6:if_inet6:[$($BB cat /proc/net/if_inet6 2>&1 | one)]"
echo "V6:ip6_addr:[$($BB timeout 15 ip -6 addr show dev eth0 2>&1 | one)]"
echo "V6:ip6_route:[$($BB timeout 15 ip -6 route 2>&1 | one)]"
echo "V6:proc_route6:[$($BB cat /proc/net/ipv6_route 2>&1 | one)]"

# ---- out-of-the-box external v6 ---------------------------------------------
( $BB ping6 -c 3 -W 3 fec0::2 >/tmp/ping6.out 2>/tmp/ping6.err ) &
( wget -q -O - "http://[fec0::2]:__PORT6__/v52" >/tmp/tcp6.out 2>/tmp/tcp6.err ) &
( echo PROBE6 | $BB nc -u -w 4 fec0::2 __PORT6__ >/tmp/udp6.out 2>/tmp/udp6.err ) &
# ---- v4 controls -------------------------------------------------------------
( wget -q -O - http://10.0.2.2:__PORT4__/v52 >/tmp/tcp4.out 2>/tmp/tcp4.err ) &
( echo PROBE4 | $BB nc -u -w 4 10.0.2.2 __PORT4__ >/tmp/udp4.out 2>/tmp/udp4.err ) &
( $BB nslookup example.com 10.0.2.3 >/tmp/dns4.out 2>/tmp/dns4.err ) &

# ---- REGRESSION WATCH: a GLOBAL v6 address with no route to it. Before V5-2
#      the connect failed instantly (no v6 source => EADDRNOTAVAIL). Now that
#      eth0 HAS a v6 address, does it still fail fast, or does it hang?
( S=$($BB date +%s)
  $BB timeout 30 wget -q -O - "http://[2606:4700:4700::1111]:80/" >/tmp/glob6.out 2>/tmp/glob6.err
  E=$($BB date +%s)
  echo "elapsed=$((E-S))s" >/tmp/glob6.secs ) &

$BB sleep 55
echo "V6:ping6:[$($BB cat /tmp/ping6.out 2>/dev/null | $BB tail -2 | one)]"
say tcp6
say udp6
say tcp4
say udp4
say dns4
say glob6
echo "V6:glob6_time:[$($BB cat /tmp/glob6.secs 2>/dev/null | one)]"
echo "V6:end:[v52-acceptance]"
