#!/usr/bin/env python3
"""Sample guest PC via QEMU human monitor (unix socket) for TCG profiling.

Usage: pc-sample.py <monitor.sock> <n_samples> <interval_s> > pcs.txt
Prints one hex pc per line.
"""
import socket, sys, time, re

sock_path, n, interval = sys.argv[1], int(sys.argv[2]), float(sys.argv[3])

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(sock_path)
s.settimeout(2.0)

def drain():
    buf = b""
    try:
        while True:
            chunk = s.recv(65536)
            if not chunk:
                break
            buf += chunk
            if buf.rstrip().endswith(b"(qemu)"):
                break
    except socket.timeout:
        pass
    return buf.decode(errors="replace")

drain()  # banner
pc_re = re.compile(r"\bpc\s+([0-9a-fx]+)", re.I)
for i in range(n):
    s.sendall(b"info registers\n")
    out = drain()
    m = pc_re.search(out)
    if m:
        print(m.group(1))
    time.sleep(interval)
