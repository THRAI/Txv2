#!/usr/bin/env bash
# verify-git-net.sh — independent, end-to-end verification of git on Txv2 (rv64-qemu).
#
# Sets up a real git server on the host, boots the guest kernel under QEMU, runs
# the full git pipeline inside the guest, and prints PASS/FAIL per check with the
# raw evidence. Nothing is faked: every result is the guest's own git output,
# and the raw serial log is left on disk for inspection.
#
# Usage:   bash tools/verify-git-net-la64.sh
#          TX_GIT_NET_TIMEOUT=360 bash tools/verify-git-net-la64.sh
# Needs:   python3, openssl, git, qemu-system-loongarch64, debugfs (e2fsprogs) on the host.
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
K="$ROOT/target/loongarch64-unknown-none-softfloat/debug/tx-kernel-loongarch64-qemu-virt"
IMG="$ROOT/local-images/alpine-linux-loongarch64-ext4fs.img"
WORK="$(mktemp -d /tmp/verifygit-XXXXXX)"
HTTP_PORT=$(( (RANDOM % 2000) + 19000 ))
HTTPS_PORT=$(( HTTP_PORT + 1 ))
MARKER="VERIFY-MARKER-$$-$(date +%s)"
PUSHMARK="PUSH-MARKER-$$"
SERIAL="$WORK/serial.log"
HTTP_PID=""; HTTPS_PID=""
QEMU_TIMEOUT="${TX_GIT_NET_TIMEOUT:-300}"

cleanup() { [ -n "$HTTP_PID" ] && kill "$HTTP_PID" 2>/dev/null; [ -n "$HTTPS_PID" ] && kill "$HTTPS_PID" 2>/dev/null; }
trap cleanup EXIT

say()  { printf '%s\n' "$*"; }
pass=0; fail=0
check() { # check "<name>" "<condition-result 0/1>" "<evidence>"
  if [ "$2" = "0" ]; then printf '  [PASS] %-22s %s\n' "$1" "$3"; pass=$((pass+1));
  else printf '  [FAIL] %-22s %s\n' "$1" "$3"; fail=$((fail+1)); fi
}

say "== Txv2 git verification =="
say "worktree : $ROOT"
[ -f "$IMG" ] || { say "FATAL: image not found: $IMG"; exit 2; }
if [ ! -f "$K" ]; then
  say "kernel ELF missing — building (cargo xtask build --target la64-qemu)..."
  ( cd "$ROOT" && cargo xtask build --target la64-qemu ) >"$WORK/build.log" 2>&1 \
    || { say "FATAL: build failed (see $WORK/build.log)"; exit 2; }
fi
say "kernel   : $K"
say "ports    : http=$HTTP_PORT https=$HTTPS_PORT   marker=$MARKER"
say ""

# --- host: a real bare repo seeded with a known marker, + self-signed cert ---
SH="$WORK/srv"; mkdir -p "$SH"
git init -q --bare -b master "$SH/test.git"
git -C "$SH/test.git" config http.receivepack true
SEED="$WORK/seed"; git init -q -b master "$SEED"
git -C "$SEED" config user.email v@v; git -C "$SEED" config user.name v
printf '%s\n' "$MARKER" > "$SEED/README"
git -C "$SEED" add -A; git -C "$SEED" commit -qm "C1 base"
git -C "$SEED" remote add origin "$SH/test.git"; git -C "$SEED" push -q origin master
openssl req -x509 -newkey rsa:2048 -keyout "$SH/key.pem" -out "$SH/cert.pem" -days 5 -nodes \
  -subj "/CN=txverify.test" \
  -addext "subjectAltName=DNS:txverify.test,IP:10.0.2.2,IP:127.0.0.1" >/dev/null 2>&1

# --- host: minimal smart-HTTP(S) git server (shells out to git-http-backend) ---
cat > "$WORK/srv.py" <<'PYEOF'
import http.server, subprocess, os, sys, ssl
ROOT, PORT = sys.argv[1], int(sys.argv[2])
CERT = sys.argv[3] if len(sys.argv) > 3 else None
for c in ("/usr/lib/git-core/git-http-backend", "/usr/libexec/git-core/git-http-backend"):
    if os.path.exists(c):
        BACKEND = c; break
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self): self.cgi()
    def do_POST(self): self.cgi()
    def log_message(self, *a): pass
    def cgi(self):
        p, _, q = self.path.partition("?")
        env = dict(os.environ, GIT_HTTP_EXPORT_ALL="1", GIT_PROJECT_ROOT=ROOT, PATH_INFO=p,
                   QUERY_STRING=q, REQUEST_METHOD=self.command, REMOTE_USER="git",
                   REMOTE_ADDR="127.0.0.1", CONTENT_TYPE=self.headers.get("Content-Type", ""),
                   GIT_PROTOCOL=self.headers.get("Git-Protocol", ""))
        cl = self.headers.get("Content-Length")
        body = self.rfile.read(int(cl)) if cl else b""
        out = subprocess.run([BACKEND], input=body, env=env, capture_output=True).stdout
        hdr, sep, b = out.partition(b"\r\n\r\n")
        if not sep: hdr, sep, b = out.partition(b"\n\n")
        st, hs = 200, []
        for line in hdr.split(b"\n"):
            line = line.strip()
            if not line: continue
            k, _, v = line.partition(b":"); k, v = k.strip(), v.strip()
            if k.lower() == b"status":
                try: st = int(v.split()[0])
                except Exception: pass
            else: hs.append((k.decode(), v.decode()))
        self.send_response(st)
        for k, v in hs: self.send_header(k, v)
        self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)
srv = http.server.ThreadingHTTPServer(("0.0.0.0", PORT), H)
if CERT:
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER); ctx.load_cert_chain(CERT, sys.argv[4])
    srv.socket = ctx.wrap_socket(srv.socket, server_side=True)
srv.serve_forever()
PYEOF
python3 "$WORK/srv.py" "$SH" "$HTTP_PORT" >/dev/null 2>&1 & HTTP_PID=$!
python3 "$WORK/srv.py" "$SH" "$HTTPS_PORT" "$SH/cert.pem" "$SH/key.pem" >/dev/null 2>&1 & HTTPS_PID=$!
sleep 2
git ls-remote "http://127.0.0.1:$HTTP_PORT/test.git" >/dev/null 2>&1 && say "host: http server up" || say "host: http server DOWN"
GIT_SSL_NO_VERIFY=1 git ls-remote "https://127.0.0.1:$HTTPS_PORT/test.git" >/dev/null 2>&1 && say "host: https server up" || say "host: https server DOWN"

# --- guest script (ports/marker substituted in below) ---
cat > "$WORK/tx-run.sh" <<'GUESTEOF'
BB=/musl/bin/busybox
export GIT_PAGER=cat HOME=/musl/root GIT_EXEC_PATH=/musl/usr/libexec/git-core GIT_TEMPLATE_DIR= \
       GIT_SSL_NO_VERIFY=true GIT_CURL_VERBOSE=1 PATH=/musl/usr/bin:/musl/bin:/usr/bin:/bin
G="git -c gc.auto=0 -c maintenance.auto=false -c http.sslVerify=false -c user.email=g@g -c user.name=guest"
$BB mkdir -p /etc 2>/dev/null
echo "nameserver 10.0.2.3" > /etc/resolv.conf
echo "10.0.2.2 txverify.test" >> /etc/hosts
echo "VG:git_version:[$(git --version 2>&1 | $BB head -1)]"
# Task1: local init/add/commit/log with a real tracked file (cp avoids the shell ext4-write bug)
$BB mkdir -p /musl/root/t1; cd /musl/root/t1
echo "LOCAL-FILE-CONTENT" > /tmp/lf; $BB cp /tmp/lf tracked.txt
$G init -q . >/dev/null 2>&1; $G add tracked.txt; $G commit -qm "LOCAL-COMMIT-MARKER" >/dev/null 2>&1
echo "VG:local_log:[$($G log --oneline 2>&1 | $BB head -1)]"
echo "VG:local_content:[$($G cat-file -p HEAD:tracked.txt 2>&1)]"
cd /musl/root
echo "VG:http_clone_out:[$($BB timeout 60 $G clone http://txverify.test:__HTTP_PORT__/test.git H 2>&1 | $BB tail -1)]"
echo "VG:http_readme:[$($G -C /musl/root/H cat-file -p HEAD:README 2>&1)]"
echo "VG:https_clone_out:[$($BB timeout 90 $G clone https://txverify.test:__HTTPS_PORT__/test.git S 2>&1 | $BB tail -1)]"
echo "VG:https_readme:[$($G -C /musl/root/S cat-file -p HEAD:README 2>&1)]"
# push (empty commit avoids the shell ext4-write-content bug; tests push transport)
cd /musl/root/H
$G commit -q --allow-empty -m "__PUSHMARK__" >/dev/null 2>&1
echo "VG:push_out:[$($BB timeout 60 $G push origin master 2>&1 | $BB tail -1)]"
# pull: S was cloned before the push above → pulling must fast-forward to the pushed commit
echo "VG:pull_out:[$($BB timeout 60 $G -C /musl/root/S -c pull.ff=only pull 2>&1 | $BB tail -1)]"
echo "VG:pull_log:[$($G -C /musl/root/S log --oneline 2>&1 | $BB head -1)]"
# DNS: query the SLIRP DNS directly. Do not couple this witness to whether an
# unrelated public host accepts or rejects a random high TCP port.
$BB timeout 30 $BB nslookup github.com 10.0.2.3 >/tmp/dns.txt 2>&1
dns_rc=$?
echo "VG:dns:[rc=$dns_rc $($BB grep -ai '^Name:.*github.com' /tmp/dns.txt 2>&1 | $BB head -1)]"
echo "VG:END"
GUESTEOF
sed -i "s/__HTTP_PORT__/$HTTP_PORT/g; s/__HTTPS_PORT__/$HTTPS_PORT/g; s/__PUSHMARK__/$PUSHMARK/g" "$WORK/tx-run.sh"

# --- boot the guest ---
cp -f "$IMG" "$WORK/disk.img"
debugfs -w -R "write $WORK/tx-run.sh /tx-run.sh" "$WORK/disk.img" >/dev/null 2>&1
say "booting guest under QEMU (≈60-120s)..."
timeout "$QEMU_TIMEOUT" qemu-system-loongarch64 -machine virt -cpu la464 -kernel "$K" -m 1152M -nographic -smp 1 \
  -drive "file=$WORK/disk.img,if=none,format=raw,id=x0,file.locking=off" \
  -device virtio-blk-pci-non-transitional,drive=x0 \
  -device virtio-net-pci,netdev=net -netdev user,id=net \
  -no-reboot -rtc base=utc \
  -fw_cfg "name=opt/tx.cmdline,string=tx.runsh=/musl/tx-run.sh console=ttyS0" \
  -append "tx.runsh=/musl/tx-run.sh console=ttyS0" > "$SERIAL" 2>&1
say ""

g() { grep -a "^VG:$1:" "$SERIAL" 2>/dev/null | sed "s/^VG:$1://" | head -1; }
say "== results =="
v=$(g git_version);   case "$v" in *git\ version*) check "Task0 git binary" 0 "$v";; *) check "Task0 git binary" 1 "$v";; esac
v=$(g local_log);     case "$v" in *LOCAL-COMMIT-MARKER*) check "Task1 init/add/commit" 0 "$v";; *) check "Task1 init/add/commit" 1 "$v";; esac
v=$(g local_content); case "$v" in *LOCAL-FILE-CONTENT*) check "Task1 file content" 0 "$v";; *) check "Task1 file content" 1 "$v";; esac
v=$(g http_readme);   case "$v" in *$MARKER*) check "Task2 clone (HTTP)" 0 "$v";; *) check "Task2 clone (HTTP)" 1 "$v";; esac
v=$(g https_readme);  case "$v" in *$MARKER*) check "Task2 clone (HTTPS/TLS)" 0 "$v";; *) check "Task2 clone (HTTPS/TLS)" 1 "$v";; esac
# push: verify the host bare repo actually received the guest's commit
if git -C "$SH/test.git" log --format=%s 2>/dev/null | grep -q "$PUSHMARK"; then
  check "Task2 push" 0 "host bare repo received '$PUSHMARK'"
else
  check "Task2 push" 1 "push_out=$(g push_out)"
fi
v=$(g pull_log);      case "$v" in *$PUSHMARK*) check "Task2 pull" 0 "$v";; *) check "Task2 pull" 1 "pull_out=$(g pull_out)";; esac
v=$(g dns);           case "$v" in *rc=0*github.com*) check "DNS resolution" 0 "$v";; *) check "DNS resolution" 1 "${v:-<no resolve line>}";; esac

say ""
say "== summary: $pass passed, $fail failed =="
say "raw guest serial: $SERIAL   (grep '^VG:' for the tagged checks)"
[ "$fail" = "0" ]
