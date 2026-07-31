#!/usr/bin/env -S uv run python
"""Run a command inside txKernel's busybox shell via QEMU serial console."""
import subprocess
import sys
import os
import time
import re
import signal

PROJECT = "/Users/3y/Downloads/Tx"
KERNEL = f"{PROJECT}/target/riscv64gc-unknown-none-elf/debug/tx-kernel-riscv64-qemu-virt"
BIOS = f"{PROJECT}/external/opensbi-silent/fw_dynamic.bin"
INITRD = f"{PROJECT}/target/images/busybox-initramfs-rv64-qemu.cpio"
TTY = f"{PROJECT}/target/ttyS0"

# First, copy pthread_test into rootfs and rebuild CPIO
rootfs = f"{PROJECT}/target/rootfs/busybox-musl-rv64-qemu"
os.makedirs(f"{rootfs}/bin", exist_ok=True)

# Build the CPIO (it gets wiped, so we need to be quick)
# Better approach: rebuild AFTER xtask runs
subprocess.run([
    "cargo", "xtask", "image", "cpio", "--profile", "busybox",
], cwd=PROJECT, capture_output=True)

# Copy test binary in
subprocess.run(["cp", f"{PROJECT}/tools/shell-tests/pthread_test_small", f"{rootfs}/bin/pthread_test"])
subprocess.run(["chmod", "+x", f"{rootfs}/bin/pthread_test"])

# Rebuild CPIO manually (use same approach as xtask but from modified rootfs)
subprocess.run(
    f"cd '{rootfs}' && find . -print | cpio -o -H newc > '{PROJECT}/target/images/busybox-initramfs-rv64-qemu.cpio'",
    shell=True, cwd=PROJECT
)

print("CPIO rebuilt with pthread_test")

# Start QEMU with expect-like interaction
# Use a pty pair
import pty
master, slave = pty.openpty()
tty_name = os.ttyname(slave)

qemu = subprocess.Popen([
    "qemu-system-riscv64",
    "-machine", "virt",
    "-m", "256M",
    "-smp", "4",
    "-accel", "tcg,thread=multi",
    "-display", "none",
    "-monitor", "none",
    "-serial", f"file:{PROJECT}/target/qemu-test.serial.log",
    "-chardev", f"serial,id=ttyS1,path={tty_name}",
    "-device", "pci-serial-2x,chardev1=ttyS1",
    "-no-reboot",
    "-kernel", KERNEL,
    "-bios", BIOS,
    "-initrd", INITRD,
    "-append", "tx.profile=busybox tx.boot.mode=busybox console=ttyS0",
    "-d", "guest_errors",
    "-D", f"{PROJECT}/target/qemu-test.log",
], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)

os.close(slave)

# Read from the master PTY
import select
buf = b""
start = time.time()
timeout = 20

try:
    while time.time() - start < timeout:
        r, _, _ = select.select([master], [], [], 1.0)
        if r:
            try:
                data = os.read(master, 4096)
                if not data:
                    break
                buf += data
                text = buf.decode('utf-8', errors='replace')
                print(text[-200:], end='', flush=True)
                
                # Look for shell prompt
                if '/ #' in text or '# ' in text:
                    # Send command
                    cmd = "/bin/pthread_test\n"
                    os.write(master, cmd.encode())
                    time.sleep(3)
                    break
                    
                # Also check for boot completion
                if 'boot:ok' in text or 'child' in text or 'parent' in text:
                    # Try sending command anyway
                    cmd = "/bin/pthread_test\n"
                    os.write(master, cmd.encode())
                    time.sleep(5)
                    break
            except OSError:
                break
        if qemu.poll() is not None:
            print(f"QEMU exited with {qemu.returncode}")
            break
    
    # Read remaining output
    time.sleep(2)
    while True:
        r, _, _ = select.select([master], [], [], 0.5)
        if r:
            try:
                data = os.read(master, 4096)
                if not data:
                    break
                buf += data
            except OSError:
                break
        else:
            break
    
    print("\n=== FULL OUTPUT ===")
    print(buf.decode('utf-8', errors='replace'))
    
finally:
    os.close(master)
    qemu.terminate()
    try:
        qemu.wait(timeout=5)
    except subprocess.TimeoutExpired:
        qemu.kill()

# Also show serial log
print("\n=== SERIAL LOG ===")
try:
    with open(f"{PROJECT}/target/qemu-test.serial.log") as f:
        print(f.read())
except:
    pass
