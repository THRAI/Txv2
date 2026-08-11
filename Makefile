# Docker convenience layer for Txv2.
# Keep command ownership in `cargo xtask`; this file is wrappers only.

SHELL := /bin/bash

DOCKER_COMPOSE ?= docker compose -f docker-compose.yml
DOCKER_SERVICE ?= oscomp
DOCKER_RUN = $(DOCKER_COMPOSE) run --rm $(DOCKER_SERVICE)
DOCKER_RUN_IT = $(DOCKER_COMPOSE) run --rm -it $(DOCKER_SERVICE)
DOCKER_BUILD_ENV = $(if $(strip $(OSCOMP_GROUPS)),-e TX_OSCOMP_GROUPS=$(OSCOMP_GROUPS),)
DOCKER_RUN_BUILD = $(DOCKER_COMPOSE) run --rm $(DOCKER_BUILD_ENV) $(DOCKER_SERVICE)

OSCOMP_DATA ?= target/oscomp/testdata
OSCOMP_SUBMIT ?= target/oscomp/submit
OSCOMP_DOCKER_IMAGE ?= zhouzhouyi/os-contest:20260510
OSCOMP_TARGET ?= rv64-qemu
OSCOMP_EXTRA ?=
HOST_CARGO_TARGET_DIR ?= target/host-cargo
OSCOMP_KERNEL_PROFILE ?= --release
DEMO_TARGET ?= rv64-qemu
DEMO_PROFILE ?= alpine
DEMO_BOOT_MODE ?= alpine
DEMO_ALPINE_ROOTFS ?= target/rootfs/alpine-rv64-qemu
DEMO_TCC_STAGE ?= target/rootfs/alpine-tcc-dev-ext4-stage
DEMO_TCC_EXT4 ?= target/images/alpine-tcc-dev-root-rv64-qemu.ext4
DEMO_CMDLINE ?= tx.mount.sdcard=0
DEMO_RUSTFLAGS ?= --cfg tx_demo_boot

.PHONY: docker-help docker-build docker-shell docker-ci docker-check docker-ci-slow \
	all all-rv all-both setup-cargo-config demo \
	docker-build-rv64 docker-build-la64 docker-image-cpio-rv64 docker-image-cpio-la64 \
	docker-image-ext4-rv64 docker-image-ext4-la64 \
	docker-qemu-rv64-smoke docker-qemu-rv64-busybox docker-qemu-la64-busybox \
	docker-run-la64-busybox docker-run-la64-busybox-smp1 docker-run-rv64-busybox \
	docker-busybox-la64 docker-oscomp-doctor docker-oscomp-prepare docker-oscomp-submit \
	docker-oscomp-run docker-oscomp-qemu \
	smp-smoke-rv64 smp-smoke-la64 smp-smoke

docker-help:
	@echo "Txv2 Docker targets:"
	@echo "  make docker-build"
	@echo "  make docker-shell"
	@echo "  make docker-ci"
	@echo "  make docker-build-rv64"
	@echo "  make docker-build-la64"
	@echo "  make docker-qemu-rv64-busybox"
	@echo "  make docker-qemu-la64-busybox"
	@echo "  make docker-run-la64-busybox-smp1"
	@echo "  make smp-smoke-rv64"
	@echo "  make smp-smoke-la64"
	@echo "  make oscomp-local-rv64-smp4"
	@echo "  make oscomp-local-la64-smp4"
	@echo "  make oscomp-local-rv64-libctest-musl-smp4"
	@echo "  make oscomp-local-la64-libctest-musl-smp4"
	@echo "  make oscomp-local-rv64-ltp-musl"
	@echo "  make oscomp-local-rv64-ltp-musl-smp4"
	@echo "  make oscomp-local-la64-ltp-musl"
	@echo "  make oscomp-local-la64-ltp-musl-smp4"
	@echo "  make oscomp-local-rv64-ltp-glibc"
	@echo "  make oscomp-local-la64-ltp-glibc"
	@echo "  make oscomp-local-la64-ltp-glibc-cases OSCOMP_LTP=open01,stat02"
	@echo "  make oscomp-local-la64-glibc OSCOMP_SUITE=basic"
	@echo "  make oscomp-local-both"
	@echo "  make ltp-batches"
	@echo "  make oscomp-local-la64-ltp-batch LTP_BATCH=submit"
	@echo "  make oscomp-local-rv64-ltp-batch LTP_BATCH=p0"
	@echo "  make oscomp-export-testcase"
	@echo "  make docker-busybox-la64"
	@echo "  make docker-oscomp-prepare docker-oscomp-submit docker-oscomp-run"
	@echo "  make demo"

# Default OSComp entry point for submit builds. Match the official autotest
# expectation by preparing both RV64 and LA64 kernels from `make all`.
# Use `make all-rv` to keep the historical single-arch lane locally.
setup-cargo-config:
	mkdir -p .cargo
	cp cargo/config.toml .cargo/config.toml

all: all-both

all-rv: setup-cargo-config
	cargo xtask build --target $(OSCOMP_TARGET) $(OSCOMP_KERNEL_PROFILE)
	cargo xtask oscomp submit --target $(OSCOMP_TARGET) --submit . $(OSCOMP_KERNEL_PROFILE)

all-both: setup-cargo-config
	cargo xtask build --target rv64-qemu $(OSCOMP_KERNEL_PROFILE)
	cargo xtask build --target la64-qemu $(OSCOMP_KERNEL_PROFILE)
	cargo xtask oscomp submit --target all --submit . $(OSCOMP_KERNEL_PROFILE)

demo:
	sh tools/demo/sqlite-tcc/install-to-rootfs.sh $(DEMO_ALPINE_ROOTFS)
	sh tools/demo/sqlite-tcc/prepare-tcc-ext4.sh $(DEMO_TCC_STAGE) $(DEMO_TCC_EXT4)
	TX_ALPINE_ROOTFS=$(DEMO_ALPINE_ROOTFS) cargo xtask image cpio --profile $(DEMO_PROFILE) --target $(DEMO_TARGET)
	RUSTFLAGS="$(strip $(RUSTFLAGS) $(DEMO_RUSTFLAGS))" cargo xtask build --target $(DEMO_TARGET)
	cargo xtask qemu --target $(DEMO_TARGET) --profile $(DEMO_PROFILE) --boot-mode $(DEMO_BOOT_MODE) --interactive --append-cmdline "$(DEMO_CMDLINE)" --extra-rv64-ext4 $(DEMO_TCC_EXT4)

docker-build:
	$(DOCKER_COMPOSE) build $(DOCKER_SERVICE)

docker-shell:
	$(DOCKER_RUN_IT) bash

docker-ci:
	$(DOCKER_RUN) cargo xtask ci

docker-check:
	$(DOCKER_RUN) cargo xtask check

docker-ci-slow:
	$(DOCKER_RUN) cargo xtask ci-slow

docker-build-rv64:
	$(DOCKER_RUN_BUILD) cargo xtask build --target rv64-qemu $(OSCOMP_KERNEL_PROFILE)

docker-build-la64:
	$(DOCKER_RUN_BUILD) cargo xtask build --target la64-qemu $(OSCOMP_KERNEL_PROFILE)

docker-image-cpio-rv64:
	$(DOCKER_RUN) cargo xtask image cpio --profile busybox --target rv64-qemu

docker-image-cpio-la64:
	$(DOCKER_RUN) cargo xtask image cpio --profile busybox --target la64-qemu

docker-image-ext4-rv64:
	$(DOCKER_RUN) cargo xtask image ext4 --profile busybox --target rv64-qemu

docker-image-ext4-la64:
	$(DOCKER_RUN) cargo xtask image ext4 --profile busybox --target la64-qemu

docker-qemu-rv64-smoke:
	$(DOCKER_RUN) cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel

docker-qemu-rv64-busybox:
	$(DOCKER_RUN_IT) cargo xtask qemu --target rv64-qemu --profile busybox --interactive

docker-qemu-la64-busybox:
	$(DOCKER_RUN_IT) cargo xtask qemu --target la64-qemu --profile busybox --interactive

docker-run-la64-busybox:
	$(DOCKER_RUN) cargo xtask build --target la64-qemu
	$(DOCKER_RUN) cargo xtask image cpio --profile busybox --target la64-qemu
	$(DOCKER_RUN) cargo xtask image ext4 --profile busybox --target la64-qemu
	$(DOCKER_RUN_IT) cargo xtask qemu --target la64-qemu --profile busybox --interactive

docker-run-la64-busybox-smp1:
	$(DOCKER_RUN) cargo xtask build --target la64-qemu
	$(DOCKER_RUN) cargo xtask image cpio --profile busybox --target la64-qemu
	$(DOCKER_RUN) cargo xtask image ext4 --profile busybox --target la64-qemu
	$(DOCKER_RUN_IT) cargo xtask qemu --target la64-qemu --profile busybox --interactive --smp 1

docker-run-rv64-busybox:
	$(DOCKER_RUN) cargo xtask build --target rv64-qemu
	$(DOCKER_RUN) cargo xtask image cpio --profile busybox --target rv64-qemu
	$(DOCKER_RUN) cargo xtask image ext4 --profile busybox --target rv64-qemu
	$(DOCKER_RUN_IT) cargo xtask qemu --target rv64-qemu --profile busybox --interactive

docker-busybox-la64:
	$(DOCKER_COMPOSE) run --rm busybox-la64

docker-oscomp-doctor:
	$(DOCKER_RUN) cargo xtask oscomp doctor

docker-oscomp-prepare:
	$(DOCKER_RUN) cargo xtask oscomp prepare --data $(OSCOMP_DATA)

docker-oscomp-submit:
	$(DOCKER_RUN) cargo xtask oscomp submit --submit $(OSCOMP_SUBMIT)

docker-oscomp-run:
	$(DOCKER_RUN) cargo xtask oscomp run --data $(OSCOMP_DATA) --submit $(OSCOMP_SUBMIT) --docker-image $(OSCOMP_DOCKER_IMAGE) $(OSCOMP_EXTRA)

docker-oscomp-qemu:
	$(DOCKER_RUN) cargo xtask oscomp qemu --target $(OSCOMP_TARGET) --data $(OSCOMP_DATA) --submit $(OSCOMP_SUBMIT) $(OSCOMP_EXTRA)

# 本地多核 reactor / AP runqueue smoke。
# 注意：xtask qemu 当前从仓库默认 target/ 目录加载 kernel；这里故意不设置
# CARGO_TARGET_DIR，避免构建到 /tmp 后 QEMU 仍运行旧内核。
SMP_SMOKE_CPUS ?= 4
SMP_SMOKE_MARKERS = smp:aps:online|smp:ipi:ok|reactor:dispatch:ipi:ok|reactor:ap-loop:ok|reactor:ap-runqueue:ok|reactor:sched:stats|boot:ok

smp-smoke-rv64:
	cargo xtask build --target rv64-qemu
	cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel --smp $(SMP_SMOKE_CPUS)
	rg "$(SMP_SMOKE_MARKERS)" target/qemu-rv64-qemu-smoke.serial.log

smp-smoke-la64:
	cargo xtask build --target la64-qemu
	cargo xtask qemu --target la64-qemu --profile smoke --expect-sentinel --smp $(SMP_SMOKE_CPUS)
	rg "$(SMP_SMOKE_MARKERS)" target/qemu-la64-qemu-smoke.serial.log

smp-smoke: smp-smoke-rv64 smp-smoke-la64

# 本地评测（不需要 docker 评测镜像）
OSCOMP_OUT_RV ?= target/oscomp/os_serial_out_rv.txt
OSCOMP_OUT_RV_SMP2 ?= target/oscomp/os_serial_out_rv_smp2.txt
OSCOMP_OUT_RV_SMP4 ?= target/oscomp/os_serial_out_rv_smp4.txt
OSCOMP_OUT_LA ?= target/oscomp/os_serial_out_la.txt
OSCOMP_OUT_LA_SMP4 ?= target/oscomp/os_serial_out_la_smp4.txt
OSCOMP_GROUPS ?=
OSCOMP_LIBCTEST ?=
OSCOMP_LTP ?=
OSCOMP_SUITE ?= ltp
LTP_MAX_RUNTIME ?=
LTP_MAX_RUNTIME_CASES ?=

# LTP local testing shortcuts:
#
# 1. Run the submit whitelist used by the default OSComp lane:
#      make oscomp-local-rv64-ltp-batch LTP_BATCH=submit
#      make oscomp-local-la64-ltp-batch LTP_BATCH=submit
#
# 2. Run the full ltp-musl image payload:
#      make oscomp-local-rv64-ltp-musl
#
# 3. Run one or a few individual LTP cases:
#      make oscomp-local-rv64 OSCOMP_LTP=umask01
#      make oscomp-local-rv64 OSCOMP_LTP=open01,stat02
#      make oscomp-local-rv64 OSCOMP_LTP=sendmsg03 LTP_MAX_RUNTIME=10
#      make oscomp-local-rv64 OSCOMP_LTP=sendmsg03,recvmsg01 LTP_MAX_RUNTIME=10 LTP_MAX_RUNTIME_CASES=sendmsg03
#
# 4. Run glibc LTP. Full-image glibc runs use the official group name;
#    focused glibc case runs use a temporary slim sdcard because the guest
#    command-line selector only supports per-case filtering for ltp-musl.
#      make oscomp-local-la64-ltp-glibc
#      make oscomp-local-la64-ltp-glibc-cases OSCOMP_LTP=open01,stat02
#
# 5. Run a Txv2 syscalls-oriented sub-batch. These batches split the
#    extracted ltp-musl case list, which is mainly /musl/ltp/runtest/syscalls.
#    LTP_BATCH=all is a syscalls sweep, not the official full OSComp run and
#    not every upstream LTP runtest module.
#      make ltp-batches
#      make ltp-batch-cases LTP_BATCH=vfs
#      make oscomp-local-rv64-ltp-batch LTP_BATCH=vfs
#
# 6. Inspect native LTP runtest modules outside syscalls. Guest-side
#    ltp-runtest:<module> execution is not wired in exec.rs yet, so these
#    targets are only useful after that selector is implemented.
#      make ltp-runtests
#      make ltp-runtest-cases LTP_RUNTEST=fs
#      make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=fs
#
# Add an outer timeout for exploratory runs, for example:
#      timeout 1800s make oscomp-local-rv64-ltp-batch LTP_BATCH=vfs
#      timeout 1800s make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=fs
LTP_BATCH ?= p0
LTP_BATCH_TOOL ?= python3 tools/ltp-batches.py
LTP_BATCH_REFRESH ?= --refresh
LTP_BATCH_ARCH_RV ?= rv64
LTP_BATCH_ARCH_LA ?= la64
LTP_RUNTEST ?= smoketest
LTP_RUNTEST_CASES ?=
LTP_RUNTEST_TOOL ?= python3 tools/ltp-runtests.py
OSCOMP_LTP_GLIBC_SIZE_MB ?= 512
OSCOMP_LTP_GLIBC_RV_DATA ?= target/oscomp/ltp-glibc-rv-focus
OSCOMP_LTP_GLIBC_LA_DATA ?= target/oscomp/ltp-glibc-la-focus
COMMA := ,
OSCOMP_LIBCTEST_GROUP = libctest-musl:$(subst $(COMMA),+,$(OSCOMP_LIBCTEST))
OSCOMP_LTP_GROUP = $(if $(strip $(OSCOMP_LTP)),ltp-musl:$(subst $(COMMA),+,$(OSCOMP_LTP)),ltp-musl)
OSCOMP_EFFECTIVE_GROUPS = $(if $(strip $(OSCOMP_LIBCTEST)),$(OSCOMP_LIBCTEST_GROUP),$(if $(strip $(OSCOMP_LTP)),$(OSCOMP_LTP_GROUP),$(OSCOMP_GROUPS)))
OSCOMP_LTP_MAX_RUNTIME_CMDLINE = $(if $(strip $(LTP_MAX_RUNTIME)),tx.ltp.max_runtime=$(LTP_MAX_RUNTIME),)
OSCOMP_LTP_MAX_RUNTIME_CASES_CMDLINE = $(if $(strip $(LTP_MAX_RUNTIME_CASES)),tx.ltp.max_runtime_cases=$(subst $(COMMA),+,$(LTP_MAX_RUNTIME_CASES)),)
OSCOMP_TEST_INIT ?= 1
OSCOMP_TEST_INITRD_RV ?= target/images/test-init-initramfs-rv64-qemu.cpio
OSCOMP_TEST_INITRD_LA ?= target/images/test-init-initramfs-la64-qemu.cpio
OSCOMP_TEST_INIT_CMDLINE = $(if $(filter 1,$(OSCOMP_TEST_INIT)),init=/tx-test-init tx.test_init=1,)
OSCOMP_CMDLINE = $(strip tx.boot.mode=oscomp $(OSCOMP_TEST_INIT_CMDLINE) tx.oscomp.observe=0 tx.oscomp.observe_dump=0 $(if $(strip $(OSCOMP_EFFECTIVE_GROUPS)),tx.oscomp.groups=$(OSCOMP_EFFECTIVE_GROUPS),) $(OSCOMP_LTP_MAX_RUNTIME_CMDLINE) $(OSCOMP_LTP_MAX_RUNTIME_CASES_CMDLINE))
OSCOMP_APPEND_RV = $(if $(filter 1,$(OSCOMP_TEST_INIT)),-initrd $(OSCOMP_TEST_INITRD_RV),) $(if $(strip $(OSCOMP_CMDLINE)),-append '$(OSCOMP_CMDLINE)',)
OSCOMP_APPEND_LA = $(if $(filter 1,$(OSCOMP_TEST_INIT)),-initrd $(OSCOMP_TEST_INITRD_LA) -fw_cfg name=opt/tx.initrd$(COMMA)file=$(OSCOMP_TEST_INITRD_LA),) $(if $(strip $(OSCOMP_CMDLINE)),-append '$(OSCOMP_CMDLINE)' -fw_cfg name=opt/tx.cmdline$(COMMA)string='$(OSCOMP_CMDLINE)',)
OSCOMP_TESTCASE_OUT ?= target/oscomp/testcase
OSCOMP_SERIAL_NORMALIZE = stdbuf -o0 tr -d '\000\r'
OSCOMP_CONSOLE_FILTER = sed -u '/^[[:space:]]*$$/d'

.PHONY: oscomp-submit oscomp-submit-both oscomp-qemu-rv64 oscomp-qemu-rv64-smp2 \
	oscomp-qemu-rv64-smp4 oscomp-build-rv64 oscomp-build-la64 oscomp-build-both \
	oscomp-qemu-la64 oscomp-qemu-la64-smp4 \
	oscomp-judge-rv64 oscomp-judge-rv64-smp2 oscomp-judge-rv64-smp4 \
	oscomp-judge-la64 oscomp-judge-la64-smp4 \
	oscomp-local-rv64 oscomp-local-rv64-smp2 oscomp-local-rv64-smp4 \
	oscomp-local-la64 oscomp-local-la64-smp4 \
	oscomp-local-both \
	oscomp-local-rv64-glibc oscomp-local-rv64-glibc-smp4 \
	oscomp-local-la64-glibc oscomp-local-la64-glibc-smp4 \
	oscomp-local-rv64-libctest-musl oscomp-local-rv64-libctest-musl-smp4 \
	oscomp-local-la64-libctest-musl oscomp-local-la64-libctest-musl-smp4 \
	oscomp-local-rv64-ltp-musl oscomp-local-rv64-ltp-musl-smp4 \
	oscomp-local-la64-ltp-musl oscomp-local-la64-ltp-musl-smp4 \
	oscomp-local-rv64-ltp-glibc oscomp-local-rv64-ltp-glibc-smp4 \
	oscomp-local-la64-ltp-glibc oscomp-local-la64-ltp-glibc-smp4 \
	oscomp-local-rv64-ltp-glibc-cases oscomp-local-la64-ltp-glibc-cases \
	ltp-batches ltp-batch-cases ltp-runtests ltp-runtest-cases \
	oscomp-local-rv64-ltp-batch oscomp-local-rv64-ltp-batch-smp4 \
	oscomp-local-la64-ltp-batch oscomp-local-la64-ltp-batch-smp4 \
	oscomp-local-rv64-ltp-runtest oscomp-local-rv64-ltp-runtest-smp4 \
	oscomp-local-la64-ltp-runtest oscomp-local-la64-ltp-runtest-smp4 \
	oscomp-export-testcase

oscomp-submit:
	CARGO_TARGET_DIR=$(HOST_CARGO_TARGET_DIR) cargo xtask oscomp submit --target $(OSCOMP_TARGET) --submit $(OSCOMP_SUBMIT) $(OSCOMP_KERNEL_PROFILE)

oscomp-submit-both:
	CARGO_TARGET_DIR=$(HOST_CARGO_TARGET_DIR) cargo xtask oscomp submit --target all --submit $(OSCOMP_SUBMIT) $(OSCOMP_KERNEL_PROFILE)

oscomp-build-rv64:
	cargo xtask build --target rv64-qemu $(OSCOMP_KERNEL_PROFILE)

oscomp-build-la64:
	cargo xtask build --target la64-qemu $(OSCOMP_KERNEL_PROFILE)

oscomp-build-both: oscomp-build-rv64 oscomp-build-la64

oscomp-submit-rv64:
	CARGO_TARGET_DIR=$(HOST_CARGO_TARGET_DIR) cargo xtask oscomp submit --target rv64-qemu --submit $(OSCOMP_SUBMIT) $(OSCOMP_KERNEL_PROFILE)

oscomp-submit-la64:
	CARGO_TARGET_DIR=$(HOST_CARGO_TARGET_DIR) cargo xtask oscomp submit --target la64-qemu --submit $(OSCOMP_SUBMIT) $(OSCOMP_KERNEL_PROFILE)

oscomp-qemu-rv64:
	$(if $(filter 1,$(OSCOMP_TEST_INIT)),cargo xtask image test-init --profile busybox --target rv64-qemu,)
	set -o pipefail; \
	qemu-system-riscv64 -machine virt \
		-kernel $(OSCOMP_SUBMIT)/kernel-rv \
		-m 1G -nographic -smp 1 -bios default \
		-drive file=$(OSCOMP_DATA)/sdcard-rv.img,if=none,format=raw,id=x0,file.locking=off \
		-device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
		-no-reboot \
		-device virtio-net-device,netdev=net -netdev user,id=net \
		-rtc base=utc \
		$(OSCOMP_APPEND_RV) \
		2>&1 | $(OSCOMP_SERIAL_NORMALIZE) | tee $(OSCOMP_OUT_RV) | $(OSCOMP_CONSOLE_FILTER)

oscomp-qemu-rv64-smp2:
	mkdir -p $(dir $(OSCOMP_OUT_RV_SMP2))
	$(if $(filter 1,$(OSCOMP_TEST_INIT)),cargo xtask image test-init --profile busybox --target rv64-qemu,)
	set -o pipefail; \
	qemu-system-riscv64 -machine virt \
		-kernel $(OSCOMP_SUBMIT)/kernel-rv \
		-m 1G -nographic -smp 2 -bios default \
		-drive file=$(OSCOMP_DATA)/sdcard-rv.img,if=none,format=raw,id=x0,file.locking=off \
		-device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
		-no-reboot \
		-device virtio-net-device,netdev=net -netdev user,id=net \
		-rtc base=utc \
		$(OSCOMP_APPEND_RV) \
		2>&1 | $(OSCOMP_SERIAL_NORMALIZE) | tee $(OSCOMP_OUT_RV_SMP2) | $(OSCOMP_CONSOLE_FILTER)

oscomp-qemu-rv64-smp4:
	mkdir -p $(dir $(OSCOMP_OUT_RV_SMP4))
	$(if $(filter 1,$(OSCOMP_TEST_INIT)),cargo xtask image test-init --profile busybox --target rv64-qemu,)
	set -o pipefail; \
	qemu-system-riscv64 -machine virt \
		-kernel $(OSCOMP_SUBMIT)/kernel-rv \
		-m 1G -nographic -smp 4 -bios default \
		-drive file=$(OSCOMP_DATA)/sdcard-rv.img,if=none,format=raw,id=x0,file.locking=off \
		-device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
		-no-reboot \
		-device virtio-net-device,netdev=net -netdev user,id=net \
		-rtc base=utc \
		$(OSCOMP_APPEND_RV) \
		2>&1 | $(OSCOMP_SERIAL_NORMALIZE) | tee $(OSCOMP_OUT_RV_SMP4) | $(OSCOMP_CONSOLE_FILTER)

oscomp-qemu-la64:
	$(if $(filter 1,$(OSCOMP_TEST_INIT)),cargo xtask image test-init --profile busybox --target la64-qemu,)
	set -o pipefail; \
	qemu-system-loongarch64 \
		-kernel $(OSCOMP_SUBMIT)/kernel-la \
		-m 1G -nographic -smp 1 \
		-drive file=$(OSCOMP_DATA)/sdcard-la.img,if=none,format=raw,id=x0,file.locking=off \
		-device virtio-blk-pci,drive=x0 \
		-no-reboot \
		-device virtio-net-pci,netdev=net0 -netdev user,id=net0 \
		-rtc base=utc \
		$(OSCOMP_APPEND_LA) \
		2>&1 | $(OSCOMP_SERIAL_NORMALIZE) | tee $(OSCOMP_OUT_LA) | $(OSCOMP_CONSOLE_FILTER)

oscomp-qemu-la64-smp4:
	mkdir -p $(dir $(OSCOMP_OUT_LA_SMP4))
	$(if $(filter 1,$(OSCOMP_TEST_INIT)),cargo xtask image test-init --profile busybox --target la64-qemu,)
	set -o pipefail; \
	qemu-system-loongarch64 \
		-kernel $(OSCOMP_SUBMIT)/kernel-la \
		-m 1G -nographic -smp 4 \
		-drive file=$(OSCOMP_DATA)/sdcard-la.img,if=none,format=raw,id=x0,file.locking=off \
		-device virtio-blk-pci,drive=x0 \
		-no-reboot \
		-device virtio-net-pci,netdev=net0 -netdev user,id=net0 \
		-rtc base=utc \
		$(OSCOMP_APPEND_LA) \
		2>&1 | $(OSCOMP_SERIAL_NORMALIZE) | tee $(OSCOMP_OUT_LA_SMP4) | $(OSCOMP_CONSOLE_FILTER)

oscomp-judge-rv64:
	python3 tools/oscomp-judge.py $(OSCOMP_OUT_RV) $(OSCOMP_DATA)

oscomp-judge-rv64-smp2:
	@test -f $(OSCOMP_OUT_RV_SMP2) || { \
		echo "missing $(OSCOMP_OUT_RV_SMP2)"; \
		echo "run: make oscomp-local-rv64-smp2"; \
		exit 1; \
	}
	python3 tools/oscomp-judge.py $(OSCOMP_OUT_RV_SMP2) $(OSCOMP_DATA)

oscomp-judge-rv64-smp4:
	@test -f $(OSCOMP_OUT_RV_SMP4) || { \
		echo "missing $(OSCOMP_OUT_RV_SMP4)"; \
		echo "run: make oscomp-local-rv64-smp4"; \
		exit 1; \
	}
	python3 tools/oscomp-judge.py $(OSCOMP_OUT_RV_SMP4) $(OSCOMP_DATA)

oscomp-judge-la64:
	python3 tools/oscomp-judge.py $(OSCOMP_OUT_LA) $(OSCOMP_DATA)

oscomp-judge-la64-smp4:
	@test -f $(OSCOMP_OUT_LA_SMP4) || { \
		echo "missing $(OSCOMP_OUT_LA_SMP4)"; \
		echo "run: make oscomp-local-la64-smp4"; \
		exit 1; \
	}
	python3 tools/oscomp-judge.py $(OSCOMP_OUT_LA_SMP4) $(OSCOMP_DATA)

oscomp-local-rv64: oscomp-build-rv64 oscomp-submit-rv64 oscomp-qemu-rv64 oscomp-judge-rv64

oscomp-local-rv64-smp4: oscomp-build-rv64 oscomp-submit-rv64 oscomp-qemu-rv64-smp4 oscomp-judge-rv64-smp4

oscomp-local-rv64-smp2: docker-build-rv64 docker-oscomp-prepare oscomp-submit-rv64 oscomp-qemu-rv64-smp2 oscomp-judge-rv64-smp2

oscomp-local-la64: oscomp-build-la64 oscomp-submit-la64 oscomp-qemu-la64 oscomp-judge-la64

oscomp-local-la64-smp4: oscomp-build-la64 oscomp-submit-la64 oscomp-qemu-la64-smp4 oscomp-judge-la64-smp4

oscomp-local-both: oscomp-local-rv64 oscomp-local-la64

oscomp-local-rv64-glibc:
	$(MAKE) oscomp-local-rv64 OSCOMP_GROUPS=$(OSCOMP_SUITE)-glibc

oscomp-local-rv64-glibc-smp4:
	$(MAKE) oscomp-local-rv64-smp4 OSCOMP_GROUPS=$(OSCOMP_SUITE)-glibc

oscomp-local-la64-glibc:
	$(MAKE) oscomp-local-la64 OSCOMP_GROUPS=$(OSCOMP_SUITE)-glibc

oscomp-local-la64-glibc-smp4:
	$(MAKE) oscomp-local-la64-smp4 OSCOMP_GROUPS=$(OSCOMP_SUITE)-glibc

oscomp-local-rv64-libctest-musl:
	$(MAKE) oscomp-local-rv64 OSCOMP_GROUPS=libctest-musl

oscomp-local-rv64-libctest-musl-smp4:
	$(MAKE) oscomp-local-rv64-smp4 OSCOMP_GROUPS=libctest-musl

oscomp-local-la64-libctest-musl:
	$(MAKE) oscomp-local-la64 OSCOMP_GROUPS=libctest-musl

oscomp-local-la64-libctest-musl-smp4:
	$(MAKE) oscomp-local-la64-smp4 OSCOMP_GROUPS=libctest-musl

oscomp-local-rv64-ltp-musl:
	$(MAKE) oscomp-local-rv64 OSCOMP_GROUPS=$(OSCOMP_LTP_GROUP)

oscomp-local-rv64-ltp-musl-smp4:
	$(MAKE) oscomp-local-rv64-smp4 OSCOMP_GROUPS=$(OSCOMP_LTP_GROUP)

oscomp-local-la64-ltp-musl:
	$(MAKE) oscomp-local-la64 OSCOMP_GROUPS=$(OSCOMP_LTP_GROUP)

oscomp-local-la64-ltp-musl-smp4:
	$(MAKE) oscomp-local-la64-smp4 OSCOMP_GROUPS=$(OSCOMP_LTP_GROUP)

oscomp-local-rv64-ltp-glibc:
	$(MAKE) oscomp-local-rv64 OSCOMP_GROUPS=ltp-glibc

oscomp-local-rv64-ltp-glibc-smp4:
	$(MAKE) oscomp-local-rv64-smp4 OSCOMP_GROUPS=ltp-glibc

oscomp-local-la64-ltp-glibc:
	$(MAKE) oscomp-local-la64 OSCOMP_GROUPS=ltp-glibc

oscomp-local-la64-ltp-glibc-smp4:
	$(MAKE) oscomp-local-la64-smp4 OSCOMP_GROUPS=ltp-glibc

oscomp-local-rv64-ltp-glibc-cases: docker-build-rv64 docker-oscomp-prepare oscomp-submit-rv64
	@test -n "$(strip $(OSCOMP_LTP))" || { \
		echo "usage: make $@ OSCOMP_LTP=open01,stat02"; \
		exit 1; \
	}
	mkdir -p $(OSCOMP_LTP_GLIBC_RV_DATA)
	cargo xtask oscomp slim-sdcard \
		--suite ltp-glibc \
		--ltp-cases $(OSCOMP_LTP) \
		--output $(OSCOMP_LTP_GLIBC_RV_DATA)/sdcard-rv.img \
		--size-mb $(OSCOMP_LTP_GLIBC_SIZE_MB)
	cargo xtask oscomp qemu \
		--target rv64-qemu \
		--data $(OSCOMP_LTP_GLIBC_RV_DATA) \
		--submit $(OSCOMP_SUBMIT) \
		--boot-suite ltp-glibc
	cargo xtask oscomp score \
		--target rv64-qemu \
		--data $(OSCOMP_DATA) \
		--suite ltp-glibc

oscomp-local-la64-ltp-glibc-cases: docker-build-la64 docker-oscomp-prepare oscomp-submit-la64
	@test -n "$(strip $(OSCOMP_LTP))" || { \
		echo "usage: make $@ OSCOMP_LTP=open01,stat02"; \
		exit 1; \
	}
	mkdir -p $(OSCOMP_LTP_GLIBC_LA_DATA)
	cargo xtask oscomp slim-sdcard \
		--source $(OSCOMP_DATA)/sdcard-la.img \
		--suite ltp-glibc \
		--ltp-cases $(OSCOMP_LTP) \
		--output $(OSCOMP_LTP_GLIBC_LA_DATA)/sdcard-la.img \
		--size-mb $(OSCOMP_LTP_GLIBC_SIZE_MB)
	cargo xtask oscomp qemu \
		--target la64-qemu \
		--data $(OSCOMP_LTP_GLIBC_LA_DATA) \
		--submit $(OSCOMP_SUBMIT) \
		--boot-suite ltp-glibc
	cargo xtask oscomp score \
		--target la64-qemu \
		--data $(OSCOMP_DATA) \
		--suite ltp-glibc

# LTP grouped runs:
#
# - ltp-batch / oscomp-local-*-ltp-batch:
#   Txv2-maintained batches for the extracted/syscalls-oriented LTP case list,
#   e.g.
#   p0, smoke, fd-io, vfs, vm, process, cred, signal, time, ipc, event,
#   sched, mount, heavy, aio.
#
# - ltp-runtest / oscomp-local-*-ltp-runtest:
#   LTP native runtest files outside syscalls, e.g. fs, mm, smoketest,
#   syscalls-ipc, pty, sched. The listing targets work; guest execution still
#   needs exec.rs support before these run as real OSComp groups.
ltp-batches:
	$(LTP_BATCH_TOOL) $(LTP_BATCH_REFRESH) --list

ltp-batch-cases:
	$(LTP_BATCH_TOOL) $(LTP_BATCH_REFRESH) --batch $(LTP_BATCH)

ltp-runtests:
	$(LTP_RUNTEST_TOOL) --list

ltp-runtest-cases:
	$(LTP_RUNTEST_TOOL) --module $(LTP_RUNTEST)

oscomp-local-rv64-ltp-batch:
	@cases="$$($(LTP_BATCH_TOOL) $(LTP_BATCH_REFRESH) --arch $(LTP_BATCH_ARCH_RV) --batch $(LTP_BATCH) --csv)"; \
	count="$$(case "$$cases" in "") echo 0 ;; *) printf '%s\n' "$$cases" | awk -F, '{ print NF }' ;; esac)"; \
	echo "LTP batch $(LTP_BATCH): $$count cases"; \
	test "$$count" != 0; \
	$(MAKE) oscomp-local-rv64 OSCOMP_GROUPS=ltp-batch:$(LTP_BATCH)

oscomp-local-rv64-ltp-runtest:
	@cases="$$($(LTP_RUNTEST_TOOL) --module $(LTP_RUNTEST) --csv)"; \
	count="$$(case "$$cases" in "") echo 0 ;; *) printf '%s\n' "$$cases" | awk -F, '{ print NF }' ;; esac)"; \
	echo "LTP runtest $(LTP_RUNTEST): $$count entries"; \
	test "$$count" != 0; \
	group="ltp-runtest:$(LTP_RUNTEST)"; \
	if [ -n "$(strip $(LTP_RUNTEST_CASES))" ]; then group="$$group:$(subst $(COMMA),+,$(LTP_RUNTEST_CASES))"; fi; \
	$(MAKE) oscomp-local-rv64 OSCOMP_GROUPS=$$group

oscomp-local-rv64-ltp-runtest-smp4:
	@cases="$$($(LTP_RUNTEST_TOOL) --module $(LTP_RUNTEST) --csv)"; \
	count="$$(case "$$cases" in "") echo 0 ;; *) printf '%s\n' "$$cases" | awk -F, '{ print NF }' ;; esac)"; \
	echo "LTP runtest $(LTP_RUNTEST): $$count entries"; \
	test "$$count" != 0; \
	group="ltp-runtest:$(LTP_RUNTEST)"; \
	if [ -n "$(strip $(LTP_RUNTEST_CASES))" ]; then group="$$group:$(subst $(COMMA),+,$(LTP_RUNTEST_CASES))"; fi; \
	$(MAKE) oscomp-local-rv64-smp4 OSCOMP_GROUPS=$$group

oscomp-local-rv64-ltp-batch-smp4:
	@cases="$$($(LTP_BATCH_TOOL) $(LTP_BATCH_REFRESH) --arch $(LTP_BATCH_ARCH_RV) --batch $(LTP_BATCH) --csv)"; \
	count="$$(case "$$cases" in "") echo 0 ;; *) printf '%s\n' "$$cases" | awk -F, '{ print NF }' ;; esac)"; \
	echo "LTP batch $(LTP_BATCH): $$count cases"; \
	test "$$count" != 0; \
	$(MAKE) oscomp-local-rv64-smp4 OSCOMP_GROUPS=ltp-batch:$(LTP_BATCH)

oscomp-local-la64-ltp-batch:
	@cases="$$($(LTP_BATCH_TOOL) $(LTP_BATCH_REFRESH) --arch $(LTP_BATCH_ARCH_LA) --batch $(LTP_BATCH) --csv)"; \
	count="$$(case "$$cases" in "") echo 0 ;; *) printf '%s\n' "$$cases" | awk -F, '{ print NF }' ;; esac)"; \
	echo "LTP batch $(LTP_BATCH): $$count cases"; \
	test "$$count" != 0; \
	$(MAKE) oscomp-local-la64 OSCOMP_GROUPS=ltp-batch:$(LTP_BATCH)

oscomp-local-la64-ltp-runtest:
	@cases="$$($(LTP_RUNTEST_TOOL) --module $(LTP_RUNTEST) --csv)"; \
	count="$$(case "$$cases" in "") echo 0 ;; *) printf '%s\n' "$$cases" | awk -F, '{ print NF }' ;; esac)"; \
	echo "LTP runtest $(LTP_RUNTEST): $$count entries"; \
	test "$$count" != 0; \
	group="ltp-runtest:$(LTP_RUNTEST)"; \
	if [ -n "$(strip $(LTP_RUNTEST_CASES))" ]; then group="$$group:$(subst $(COMMA),+,$(LTP_RUNTEST_CASES))"; fi; \
	$(MAKE) oscomp-local-la64 OSCOMP_GROUPS=$$group

oscomp-local-la64-ltp-runtest-smp4:
	@cases="$$($(LTP_RUNTEST_TOOL) --module $(LTP_RUNTEST) --csv)"; \
	count="$$(case "$$cases" in "") echo 0 ;; *) printf '%s\n' "$$cases" | awk -F, '{ print NF }' ;; esac)"; \
	echo "LTP runtest $(LTP_RUNTEST): $$count entries"; \
	test "$$count" != 0; \
	group="ltp-runtest:$(LTP_RUNTEST)"; \
	if [ -n "$(strip $(LTP_RUNTEST_CASES))" ]; then group="$$group:$(subst $(COMMA),+,$(LTP_RUNTEST_CASES))"; fi; \
	$(MAKE) oscomp-local-la64-smp4 OSCOMP_GROUPS=$$group

oscomp-local-la64-ltp-batch-smp4:
	@cases="$$($(LTP_BATCH_TOOL) $(LTP_BATCH_REFRESH) --arch $(LTP_BATCH_ARCH_LA) --batch $(LTP_BATCH) --csv)"; \
	count="$$(case "$$cases" in "") echo 0 ;; *) printf '%s\n' "$$cases" | awk -F, '{ print NF }' ;; esac)"; \
	echo "LTP batch $(LTP_BATCH): $$count cases"; \
	test "$$count" != 0; \
	$(MAKE) oscomp-local-la64-smp4 OSCOMP_GROUPS=ltp-batch:$(LTP_BATCH)

oscomp-export-testcase:
	tools/oscomp-extract-testcase.sh $(OSCOMP_DATA) $(OSCOMP_TESTCASE_OUT)
