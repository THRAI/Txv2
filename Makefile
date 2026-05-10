# Docker convenience layer for Txv2.
# Keep command ownership in `cargo xtask`; this file is wrappers only.

DOCKER_COMPOSE ?= docker compose -f docker-compose.yml
DOCKER_SERVICE ?= oscomp
DOCKER_RUN = $(DOCKER_COMPOSE) run --rm $(DOCKER_SERVICE)

OSCOMP_DATA ?= target/oscomp/testdata
OSCOMP_SUBMIT ?= target/oscomp/submit
OSCOMP_DOCKER_IMAGE ?= zhouzhouyi/os-contest:20260104
OSCOMP_TARGET ?= rv64-qemu
OSCOMP_EXTRA ?=

.PHONY: docker-help docker-build docker-shell docker-ci docker-check docker-ci-slow \
	docker-build-rv64 docker-build-la64 docker-image-cpio-rv64 docker-image-cpio-la64 \
	docker-qemu-rv64-smoke docker-qemu-rv64-busybox docker-qemu-la64-busybox \
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
	@echo "  make docker-busybox-la64"
	@echo "  make docker-oscomp-prepare docker-oscomp-submit docker-oscomp-run"

docker-build:
	$(DOCKER_COMPOSE) build $(DOCKER_SERVICE)

docker-shell:
	$(DOCKER_RUN) bash

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

docker-qemu-rv64-smoke:
	$(DOCKER_RUN) cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel

docker-qemu-rv64-busybox:
	$(DOCKER_RUN) cargo xtask qemu --target rv64-qemu --profile busybox --interactive

docker-qemu-la64-busybox:
	$(DOCKER_RUN) cargo xtask qemu --target la64-qemu --profile busybox --interactive

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
