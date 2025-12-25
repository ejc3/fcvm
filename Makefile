SHELL := /bin/bash

# Paths (can be overridden via environment)
FUSE_BACKEND_RS ?= /home/ubuntu/fuse-backend-rs
FUSER ?= /home/ubuntu/fuser

# Container settings
CONTAINER_TAG := fcvm-test:latest
CONTAINER_ARCH ?= aarch64

# Test options: FILTER=pattern STREAM=1 LIST=1
FILTER ?=
ifeq ($(STREAM),1)
NEXTEST_CAPTURE := --no-capture
endif
ifeq ($(LIST),1)
NEXTEST_CMD := list
else
NEXTEST_CMD := run
endif

# Architecture detection
ARCH := $(shell uname -m)
ifeq ($(ARCH),aarch64)
MUSL_TARGET := aarch64-unknown-linux-musl
else
MUSL_TARGET := x86_64-unknown-linux-musl
endif

# Base test command
NEXTEST := CARGO_TARGET_DIR=target cargo nextest $(NEXTEST_CMD) --release

# Container run command (runs as testuser via Containerfile USER directive)
CONTAINER_RUN := podman run --rm --privileged \
	-v .:/workspace/fcvm -v $(FUSE_BACKEND_RS):/workspace/fuse-backend-rs -v $(FUSER):/workspace/fuser \
	--device /dev/fuse --device /dev/kvm \
	--ulimit nofile=65536:65536 --pids-limit=65536 -v /mnt/fcvm-btrfs:/mnt/fcvm-btrfs

.PHONY: all help build clean test test-unit test-fast test-all test-root \
	_test-unit _test-fast _test-all _test-root \
	container-build container-test container-test-unit container-test-fast container-test-all \
	container-shell container-clean setup-btrfs setup-fcvm setup-pjdfstest bench lint fmt

all: build

help:
	@echo "fcvm: make build | test-unit | test-fast | test-all | test-root"
	@echo "      make container-test-unit | container-test-fast | container-test-all"
	@echo "Options: FILTER=pattern STREAM=1 LIST=1"

build:
	@echo "==> Building..."
	CARGO_TARGET_DIR=target cargo build --release -p fcvm
	CARGO_TARGET_DIR=target cargo build --release -p fc-agent --target $(MUSL_TARGET)
	@mkdir -p target/release && cp target/$(MUSL_TARGET)/release/fc-agent target/release/fc-agent

clean:
	sudo rm -rf target

# Run-only targets (no setup deps, used by container)
_test-unit:
	$(NEXTEST) --no-default-features

_test-fast:
	$(NEXTEST) $(NEXTEST_CAPTURE) --no-default-features --features integration-fast $(FILTER)

_test-all:
	$(NEXTEST) $(NEXTEST_CAPTURE) $(FILTER)

_test-root:
	CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER='sudo -E' \
	CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER='sudo -E' \
	$(NEXTEST) $(NEXTEST_CAPTURE) --features privileged-tests $(FILTER)

# Host targets (with setup)
test-unit: build _test-unit
test-fast: setup-fcvm _test-fast
test-all: setup-fcvm _test-all
test-root: setup-fcvm setup-pjdfstest _test-root
test: test-root

# Container targets (setup on host where needed, run-only in container)
container-test-unit: container-build
	@echo "==> Running unit tests in container..."
	$(CONTAINER_RUN) $(CONTAINER_TAG) make build _test-unit

container-test-fast: container-setup-fcvm
	@echo "==> Running fast tests in container..."
	$(CONTAINER_RUN) $(CONTAINER_TAG) make _test-fast

container-test-all: container-setup-fcvm
	@echo "==> Running all tests in container..."
	$(CONTAINER_RUN) $(CONTAINER_TAG) make _test-all

container-test: container-test-all

container-build:
	@sudo mkdir -p /mnt/fcvm-btrfs 2>/dev/null || true
	podman build -t $(CONTAINER_TAG) -f Containerfile --build-arg ARCH=$(CONTAINER_ARCH) .

container-shell: container-build
	$(CONTAINER_RUN) -it $(CONTAINER_TAG) bash

container-clean:
	podman rmi $(CONTAINER_TAG) 2>/dev/null || true

# Setup targets
setup-pjdfstest:
	@if [ ! -x /tmp/pjdfstest-check/pjdfstest ]; then \
		echo '==> Building pjdfstest...'; \
		rm -rf /tmp/pjdfstest-check && \
		git clone --depth 1 https://github.com/pjd/pjdfstest /tmp/pjdfstest-check && \
		cd /tmp/pjdfstest-check && autoreconf -ifs && ./configure && make; \
	fi

setup-btrfs:
	@if ! mountpoint -q /mnt/fcvm-btrfs 2>/dev/null; then \
		echo '==> Creating btrfs loopback...'; \
		if [ ! -f /var/fcvm-btrfs.img ]; then \
			sudo truncate -s 20G /var/fcvm-btrfs.img && sudo mkfs.btrfs /var/fcvm-btrfs.img; \
		fi && \
		sudo mkdir -p /mnt/fcvm-btrfs && \
		sudo mount -o loop /var/fcvm-btrfs.img /mnt/fcvm-btrfs && \
		sudo mkdir -p /mnt/fcvm-btrfs/{kernels,rootfs,initrd,state,snapshots,vm-disks,cache} && \
		sudo chown -R $$(id -un):$$(id -gn) /mnt/fcvm-btrfs && \
		echo '==> btrfs ready at /mnt/fcvm-btrfs'; \
	fi

setup-fcvm: build setup-btrfs
	@FREE_GB=$$(df -BG /mnt/fcvm-btrfs 2>/dev/null | awk 'NR==2 {gsub("G",""); print $$4}'); \
	if [ -n "$$FREE_GB" ] && [ "$$FREE_GB" -lt 15 ]; then \
		echo "ERROR: Need 15GB on /mnt/fcvm-btrfs (have $${FREE_GB}GB)"; \
		exit 1; \
	fi
	@echo "==> Running fcvm setup..."
	./target/release/fcvm setup

# Run setup inside container (for CI - container has Firecracker)
container-setup-fcvm: container-build setup-btrfs
	@echo "==> Running fcvm setup in container..."
	$(CONTAINER_RUN) $(CONTAINER_TAG) make build _setup-fcvm

_setup-fcvm:
	@FREE_GB=$$(df -BG /mnt/fcvm-btrfs 2>/dev/null | awk 'NR==2 {gsub("G",""); print $$4}'); \
	if [ -n "$$FREE_GB" ] && [ "$$FREE_GB" -lt 15 ]; then \
		echo "ERROR: Need 15GB on /mnt/fcvm-btrfs (have $${FREE_GB}GB)"; \
		exit 1; \
	fi
	./target/release/fcvm setup

bench: build
	@echo "==> Running benchmarks..."
	sudo cargo bench -p fuse-pipe --bench throughput
	sudo cargo bench -p fuse-pipe --bench operations
	cargo bench -p fuse-pipe --bench protocol

lint:
	cargo test --test lint

fmt:
	cargo fmt
