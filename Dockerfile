# ================================================================
# txKernel (Txv2) Development & Test Container
#
# Self-contained build/test image:
# - QEMU / filesystem tools / Python tooling are in the image.
# - Rust toolchain and cross targets are installed in the image so
#   `make all` does not depend on host-mounted rustup/cargo state.
# ================================================================

FROM ubuntu:24.04

SHELL ["/bin/bash", "-o", "pipefail", "-lc"]

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential cmake \
        qemu-system-misc \
        e2fsprogs mtools dosfstools cpio \
        python3 python3-pexpect \
        make git curl ca-certificates \
        xz-utils file zip jq && \
    rm -rf /var/lib/apt/lists/*

# Install zig for cross C toolchain wrappers used by some LA64/RV64 deps.
ARG ZIG_VERSION=0.14.1
ARG ZIG_BASE_URL=https://ziglang.org/download
RUN curl --fail --location --retry 5 --retry-delay 3 \
        --connect-timeout 20 --speed-time 30 --speed-limit 1024 \
        --output /tmp/zig.tar.xz \
        "${ZIG_BASE_URL}/${ZIG_VERSION}/zig-x86_64-linux-${ZIG_VERSION}.tar.xz" && \
    tar -xJf /tmp/zig.tar.xz -C /opt && \
    ln -s "/opt/zig-x86_64-linux-${ZIG_VERSION}/zig" /usr/local/bin/zig && \
    rm -f /tmp/zig.tar.xz

# Install uv (used by shell-test and local python helpers).
ARG UV_INSTALL_URL=https://astral.sh/uv/install.sh
RUN curl --fail --location --retry 5 --retry-delay 3 \
        --connect-timeout 20 --speed-time 30 --speed-limit 1024 \
        "${UV_INSTALL_URL}" | sh
ENV PATH="/root/.local/bin:$PATH"

ARG RUST_TOOLCHAIN=nightly-2025-05-20
ARG RUSTUP_INSTALL_URL=https://sh.rustup.rs
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH="/usr/local/cargo/bin:/root/.local/bin:$PATH"

RUN curl --proto '=https' --tlsv1.2 --fail --location --retry 5 --retry-delay 3 \
        --connect-timeout 20 --speed-time 30 --speed-limit 1024 \
        "${RUSTUP_INSTALL_URL}" | \
        sh -s -- -y --profile minimal --default-toolchain "${RUST_TOOLCHAIN}" && \
    rustup component add rust-src llvm-tools-preview --toolchain "${RUST_TOOLCHAIN}" && \
    rustup target add \
        riscv64gc-unknown-none-elf \
        loongarch64-unknown-none-softfloat \
        loongarch64-unknown-none \
        --toolchain "${RUST_TOOLCHAIN}" && \
    rustup default "${RUST_TOOLCHAIN}" && \
    cargo --version && \
    rustc --version

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

WORKDIR /workspace
