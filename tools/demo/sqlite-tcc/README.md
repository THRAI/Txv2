# SQLite + vi + TCC Demo

This demo is meant to be run inside the Alpine guest. It shows:

- interactive `sqlite3`;
- `vi` editing a C source file;
- TCC compiling a musl C program inside the guest;
- the database living on the ext4 disk mounted at `/mnt/tcc`.

## Boot Shape

Use the Alpine initramfs with the TCC ext4 image attached as `vda`, and disable
the boot-time `/musl` sdcard auto-mount so the demo can mount it explicitly:

```sh
make demo
```

`make demo` installs this directory into the Alpine rootfs, rebuilds the Alpine
cpio, builds the RV64 kernel, and starts interactive QEMU with the TCC ext4
image attached.

## Guest Setup

Copy this directory into the guest image or paste the scripts manually, then run:

```sh
sh /demo/sqlite-tcc/setup-tcc.sh
```

From the host checkout, the easiest way to install the directory into the
Alpine rootfs is:

```sh
make demo
```

The setup script is idempotent. It creates `/mnt/tcc`, mounts
`/dev/block/vda`, and installs the `/usr` and `/lib` compatibility symlinks
that TCC expects. It also exposes short demo paths:

```sh
tcc      # /usr/bin/tcc -> /mnt/tcc/usr/bin/tcc
cc       # /usr/bin/cc  -> /mnt/tcc/usr/bin/tcc
/tcc     # symlink to /mnt/tcc
/mnt/tmp # symlink to /mnt/tcc/tmp
```

## Interactive Flow

First show sqlite is alive:

```sh
/usr/bin/sqlite3 :memory: "select 1+2;"
```

Then edit the C program:

```sh
vi /tmp/sqlite_threads.c
```

Paste the contents of `sqlite_threads.c`, save with `Esc`, `:wq`, then compile:

```sh
tcc /tmp/sqlite_threads.c -o /tmp/sqlite_threads
```

Run the pthread program:

```sh
/tmp/sqlite_threads
echo "tcc-program-status:$?"
```

Expected output includes:

```text
main: creating pthread
worker 3 says hello
main: pthread returned 13
tcc-program-status:0
```

Run the link-only helper:

```sh
sh /demo/sqlite-tcc/run-demo.sh
```

Expected output includes:

```text
tcc-link-status:0
tcc-linked-bin:/tmp/sqlite_threads
```

If you want to run sqlite separately, create and check an ext4-backed database:

```sh
/usr/bin/sqlite3 -batch -noheader /mnt/tmp/demo.db \
  "PRAGMA journal_mode=DELETE;
   CREATE TABLE IF NOT EXISTS log(worker TEXT, n INTEGER);
   INSERT INTO log(worker,n) VALUES('manual',1);
   SELECT 'count:' || count(*) FROM log;
   PRAGMA integrity_check;"
```

Expected:

```text
delete
count:1
ok
```

## One-Shot Flow

If this directory is available in the guest at `/demo/sqlite-tcc`, run:

```sh
sh /demo/sqlite-tcc/run-demo.sh
```

That script runs setup, copies `sqlite_threads.c` to `/tmp`, compiles it, runs
TCC, and prints the linked binary path. It does not execute the linked program
or run sqlite worker tasks.
