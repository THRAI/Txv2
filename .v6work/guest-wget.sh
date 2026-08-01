BB=/musl/bin/busybox
export PATH=/musl/usr/bin:/musl/bin:/usr/bin:/bin HOME=/musl/root
echo "W:begin:[IPC bench — pipe vs AF_UNIX socketpair]"
$BB chmod 755 /musl/ipcbench 2>/dev/null
/musl/ipcbench 2>&1 | while read -r l; do echo "W:$l"; done
echo "W:end:[]"
