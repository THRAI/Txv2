BB=/musl/bin/busybox
export PATH=/musl/sbin:/musl/usr/sbin:/musl/usr/bin:/musl/bin:/sbin:/usr/bin:/bin HOME=/musl/root
one() { $BB tr '\n' ';' ; }
say() { echo "V6:$1:[out=$($BB cat /tmp/$1.out 2>/dev/null | one)err=$($BB cat /tmp/$1.err 2>/dev/null | one)]"; }

echo "V6:begin:[v51-acceptance]"
# V5-1 acceptance still configures v6 by hand (V5-2 is what removes this).
$BB timeout 15 ip -6 addr add fec0::15/64 dev eth0 >/dev/null 2>&1
echo "V6:cfg:[$($BB cat /proc/net/if_inet6 2>&1 | one)]"

# ---- the two V5-1 acceptance probes -----------------------------------------
( echo PROBE6 | $BB nc -u -w 4 fec0::2 __PORT6__ >/tmp/udp6.out 2>/tmp/udp6.err ) &
( $BB nslookup example.com fec0::3 >/tmp/dns6.out 2>/tmp/dns6.err ) &
# ---- v4 controls (must not regress) -----------------------------------------
( echo PROBE4 | $BB nc -u -w 4 10.0.2.2 __PORT4__ >/tmp/udp4.out 2>/tmp/udp4.err ) &
( $BB nslookup example.com 10.0.2.3 >/tmp/dns4.out 2>/tmp/dns4.err ) &
( wget -q -O - http://10.0.2.2:__PORT4__/v4 >/tmp/tcp4.out 2>/tmp/tcp4.err ) &
( wget -q -O - "http://[fec0::2]:__PORT6__/v6" >/tmp/tcp6.out 2>/tmp/tcp6.err ) &
( $BB ping6 -c 2 -W 3 fec0::2 >/tmp/ping6.out 2>/tmp/ping6.err ) &

$BB sleep 45
say udp6
say dns6
say udp4
say dns4
say tcp4
say tcp6
echo "V6:ping6:[$($BB cat /tmp/ping6.out 2>/dev/null | $BB tail -2 | one)]"
echo "V6:end:[v51-acceptance]"
