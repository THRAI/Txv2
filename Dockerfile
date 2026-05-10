# ================================================================
# txKernel (Txv2) Development & Test Container
#
# Runtime image only:
# - QEMU / filesystem tools / Python tooling are in the image.
# - Rust toolchain is mounted from host via docker-compose.yml.
# ================================================================

FROM ubuntu:24.04

RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential cmake \
        qemu-system-misc \
        e2fsprogs mtools dosfstools cpio \
        python3 python3-pexpect \
        make git curl ca-certificates \
        xz-utils file zip jq && \
    rm -rf /var/lib/apt/lists/*

# Install zig for cross C toolchain wrappers used by some LA64/RV64 deps.
RUN curl -L https://ziglang.org/download/0.14.1/zig-x86_64-linux-0.14.1.tar.xz | \
    tar -xJ -C /opt && \
    ln -s /opt/zig-x86_64-linux-0.14.1/zig /usr/local/bin/zig

# Install uv (used by shell-test and local python helpers).
RUN curl -LsSf https://astral.sh/uv/install.sh | sh
ENV PATH="/root/.local/bin:$PATH"

# Cross wrappers (same contract as host setup scripts).
RUN printf '#!/bin/sh\nexec zig cc -target riscv64-linux-musl "$@"\n' \
        > /usr/local/bin/riscv64-linux-musl-cc && \
    printf '#!/bin/sh\nexec zig ar "$@"\n' \
        > /usr/local/bin/riscv64-linux-musl-ar && \
    printf '#!/bin/sh\nexec zig cc -target loongarch64-linux-musl "$@"\n' \
        > /usr/local/bin/loongarch64-linux-musl-cc && \
    printf '#!/bin/sh\nexec zig ar "$@"\n' \
        > /usr/local/bin/loongarch64-linux-musl-ar && \
    chmod +x /usr/local/bin/riscv64-linux-musl-cc \
             /usr/local/bin/riscv64-linux-musl-ar \
             /usr/local/bin/loongarch64-linux-musl-cc \
             /usr/local/bin/loongarch64-linux-musl-ar

# LA64 virtio PCI ROM compatibility path for some qemu builds.
RUN install -d /usr/share/qemu && \
    ln -sf /usr/share/qemu/qboot.rom /usr/share/qemu/efi-virtio.rom

ENV RUSTUP_HOME=/root/.rustup \
    CARGO_HOME=/root/.cargo \
    PATH="/root/.cargo/bin:/root/.local/bin:$PATH"

WORKDIR /workspace
