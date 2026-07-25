BB=/musl/bin/busybox
export PATH=/musl/sbin:/musl/usr/sbin:/musl/usr/bin:/musl/bin:/sbin:/usr/bin:/bin HOME=/musl/root
one() { $BB tr '\n' ';' ; }
say() { echo "V6:$1:[out=$($BB cat /tmp/$1.out 2>/dev/null | one)err=$($BB cat /tmp/$1.err 2>/dev/null | one)]"; }
# One external-v6 reachability probe (TCP + UDP), tagged so each phase is
# distinguishable. Detached, three files, per the harness rules.
probe() {
  t=$1
  ( wget -q -O - "http://[fec0::2]:__PORT6__/$t" >/tmp/tcp6$t.out 2>/tmp/tcp6$t.err ) &
  ( echo P6$t | $BB nc -u -w 4 fec0::2 __PORT6__ >/tmp/udp6$t.out 2>/tmp/udp6$t.err ) &
}

echo "V6:begin:[v52fix-acceptance]"

# ---- PHASE A: zero config (V5-2 boot seed only) ------------------------------
echo "V6:A_if_inet6:[$($BB cat /proc/net/if_inet6 2>&1 | one)]"
probe A
$BB sleep 25
say tcp6A
say udp6A

# ---- PHASE B: user adds a DIFFERENT v6 address. Before the newest-wins fix it
#      landed in the unroutable secondary list; now it must take the primary
#      slot (and fec0::2 correctly becomes unreachable — off-prefix, no default
#      route — which is the documented one-usable-address reality). ----------
echo "V6:B_add:[$($BB timeout 15 ip -6 addr add 2001:db8:1::15/64 dev eth0 2>&1 | one)rc=$?]"
echo "V6:B_if_inet6:[$($BB cat /proc/net/if_inet6 2>&1 | one)]"
echo "V6:B_route6:[$($BB timeout 15 ip -6 route 2>&1 | one)]"
probe B
$BB sleep 25
say tcp6B
say udp6B

# ---- PHASE C: delete it again. The demoted fec0::15 must be promoted back and
#      external v6 must WORK again (this is the end-to-end proof). -----------
echo "V6:C_del:[$($BB timeout 15 ip -6 addr del 2001:db8:1::15/64 dev eth0 2>&1 | one)rc=$?]"
echo "V6:C_if_inet6:[$($BB cat /proc/net/if_inet6 2>&1 | one)]"
echo "V6:C_route6:[$($BB timeout 15 ip -6 route 2>&1 | one)]"
probe C
$BB sleep 30
say tcp6C
say udp6C

# ---- v4 control -------------------------------------------------------------
( wget -q -O - http://10.0.2.2:__PORT4__/v4 >/tmp/tcp4.out 2>/tmp/tcp4.err ) &
$BB sleep 20
say tcp4
echo "V6:end:[v52fix-acceptance]"
