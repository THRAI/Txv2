# Docker convenience layer for Txv2.
# Keep command ownership in `cargo xtask`; this file is wrappers only.

SHELL := /bin/bash

DOCKER_COMPOSE ?= docker compose -f docker-compose.yml
DOCKER_SERVICE ?= oscomp
DOCKER_RUN = $(DOCKER_COMPOSE) run --rm $(DOCKER_SERVICE)
DOCKER_RUN_IT = $(DOCKER_COMPOSE) run --rm -it $(DOCKER_SERVICE)

OSCOMP_DATA ?= target/oscomp/testdata
OSCOMP_SUBMIT ?= target/oscomp/submit
OSCOMP_DOCKER_IMAGE ?= zhouzhouyi/os-contest:20260104
OSCOMP_TARGET ?= rv64-qemu
OSCOMP_EXTRA ?=
HOST_CARGO_TARGET_DIR ?= target/host-cargo

.PHONY: docker-help docker-build docker-shell docker-ci docker-check docker-ci-slow \
	docker-build-rv64 docker-build-la64 docker-image-cpio-rv64 docker-image-cpio-la64 \
	docker-image-ext4-rv64 docker-image-ext4-la64 \
	docker-qemu-rv64-smoke docker-qemu-rv64-busybox docker-qemu-la64-busybox \
	docker-run-la64-busybox docker-run-la64-busybox-smp1 docker-run-rv64-busybox \
	docker-busybox-la64 docker-oscomp-doctor docker-oscomp-prepare docker-oscomp-submit \
	docker-oscomp-run docker-oscomp-qemu

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
	@echo "  make docker-busybox-la64"
	@echo "  make docker-oscomp-prepare docker-oscomp-submit docker-oscomp-run"

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
	$(DOCKER_RUN) cargo xtask build --target rv64-qemu

docker-build-la64:
	$(DOCKER_RUN) cargo xtask build --target la64-qemu

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

# 本地评测（不需要 docker 评测镜像）
OSCOMP_OUT_RV ?= target/oscomp/os_serial_out_rv.txt
OSCOMP_OUT_LA ?= target/oscomp/os_serial_out_la.txt
OSCOMP_SERIAL_NORMALIZE = stdbuf -o0 tr -d '\000\r'
OSCOMP_CONSOLE_FILTER = sed -u '/^[[:space:]]*$$/d'

.PHONY: oscomp-submit oscomp-qemu-rv64 oscomp-qemu-la64 oscomp-judge-rv64 oscomp-judge-la64 oscomp-local-rv64 oscomp-local-la64

oscomp-submit:
	CARGO_TARGET_DIR=$(HOST_CARGO_TARGET_DIR) cargo xtask oscomp submit --submit $(OSCOMP_SUBMIT)

oscomp-submit-rv64:
	CARGO_TARGET_DIR=$(HOST_CARGO_TARGET_DIR) cargo xtask oscomp submit --target rv64-qemu --submit $(OSCOMP_SUBMIT)

oscomp-submit-la64:
	CARGO_TARGET_DIR=$(HOST_CARGO_TARGET_DIR) cargo xtask oscomp submit --target la64-qemu --submit $(OSCOMP_SUBMIT)

oscomp-qemu-rv64:
	set -o pipefail; \
	qemu-system-riscv64 -machine virt \
		-kernel $(OSCOMP_SUBMIT)/kernel-rv \
		-m 1G -nographic -smp 1 -bios default \
		-drive file=$(OSCOMP_DATA)/sdcard-rv.img,if=none,format=raw,id=x0,file.locking=off \
		-device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
		-no-reboot \
		-device virtio-net-device,netdev=net -netdev user,id=net \
		-rtc base=utc \
		2>&1 | $(OSCOMP_SERIAL_NORMALIZE) | tee $(OSCOMP_OUT_RV) | $(OSCOMP_CONSOLE_FILTER)

oscomp-qemu-la64:
	set -o pipefail; \
	qemu-system-loongarch64 \
		-kernel $(OSCOMP_SUBMIT)/kernel-la \
		-m 1G -nographic -smp 1 \
		-drive file=$(OSCOMP_DATA)/sdcard-la.img,if=none,format=raw,id=x0,file.locking=off \
		-device virtio-blk-pci,drive=x0 \
		-no-reboot \
		-device virtio-net-pci,netdev=net0 -netdev user,id=net0 \
		-rtc base=utc \
		2>&1 | $(OSCOMP_SERIAL_NORMALIZE) | tee $(OSCOMP_OUT_LA) | $(OSCOMP_CONSOLE_FILTER)

oscomp-judge-rv64:
	python3 tools/oscomp-judge.py $(OSCOMP_OUT_RV) $(OSCOMP_DATA)

oscomp-judge-la64:
	python3 tools/oscomp-judge.py $(OSCOMP_OUT_LA) $(OSCOMP_DATA)

oscomp-local-rv64: docker-build-rv64 docker-oscomp-prepare oscomp-submit-rv64 oscomp-qemu-rv64 oscomp-judge-rv64

oscomp-local-la64: docker-build-la64 docker-oscomp-prepare oscomp-submit-la64 oscomp-qemu-la64 oscomp-judge-la64
