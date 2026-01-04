SHELL := /bin/bash

# Brief notes (see .claude/CLAUDE.md for details):
#   FILTER=x STREAM=1 - filter tests, stream output
#   Assets are content-addressed (kernel by URL SHA, rootfs by script SHA, initrd by binary SHA)
#   Logs: /tmp/fcvm-test-logs/
.PHONY: show-notes
show-notes:
	@echo "━━━ fcvm ━━━  FILTER=$(FILTER) STREAM=$(STREAM)  Assets=SHA-cached  (see .claude/CLAUDE.md)"

# Paths (can be overridden via environment)
FUSE_BACKEND_RS ?= /home/ubuntu/fuse-backend-rs
FUSER ?= /home/ubuntu/fuser

# Container settings
CONTAINER_TAG := fcvm-test:latest
CONTAINER_ARCH ?= aarch64

# Per-mode data directories (prevents UID conflicts between test modes)
ROOT_DATA_DIR := /mnt/fcvm-btrfs/root
CONTAINER_DATA_DIR := /mnt/fcvm-btrfs/container

# Test options: FILTER=pattern STREAM=1 LIST=1 IGNORED=1
FILTER ?=
ifeq ($(IGNORED),1)
NEXTEST_IGNORED := --run-ignored=all
else
NEXTEST_IGNORED :=
endif

# Default log level: fcvm debug, suppress FUSE spam
# Override with: RUST_LOG=debug make test-root
TEST_LOG ?= fcvm=debug,health-monitor=info,fuser=warn,fuse_backend_rs=warn,passthrough=warn
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

# Optional cargo cache directory (for CI caching)
CARGO_CACHE_DIR ?=
ifneq ($(CARGO_CACHE_DIR),)
# CI mode: use cache directory for both registry and target
CARGO_CACHE_MOUNT := -v $(CARGO_CACHE_DIR)/registry:/usr/local/cargo/registry
TARGET_MOUNT := -v $(CARGO_CACHE_DIR)/target:/workspace/fcvm/target
else
# Local mode: use temp directory for target (avoids permission conflicts)
CARGO_CACHE_MOUNT :=
TARGET_MOUNT := -v /tmp/fcvm-container-target:/workspace/fcvm/target
endif

# Test log directory (mounted into container)
TEST_LOG_DIR := /tmp/fcvm-test-logs

# Container run command
# Note: Use -v instead of --device for /dev/kvm to preserve group permissions in rootless mode
# See: https://github.com/containers/podman/issues/16701
CONTAINER_RUN := podman run --rm --privileged \
	--security-opt label=disable --group-add keep-groups \
	-v .:/workspace/fcvm \
	$(TARGET_MOUNT) \
	-v $(FUSE_BACKEND_RS):/workspace/fuse-backend-rs -v $(FUSER):/workspace/fuser \
	--device /dev/fuse -v /dev/kvm:/dev/kvm \
	--ulimit nofile=65536:65536 --pids-limit=65536 -v /mnt/fcvm-btrfs:/mnt/fcvm-btrfs \
	-v $(TEST_LOG_DIR):$(TEST_LOG_DIR) $(CARGO_CACHE_MOUNT) \
	-e FCVM_DATA_DIR=$(CONTAINER_DATA_DIR)

.PHONY: all help build clean clean-test-data check-disk \
	test test-unit test-fast test-all test-root test-packaging \
	_test-unit _test-fast _test-all _test-root _setup-fcvm _bench \
	container-build container-test container-test-unit container-test-fast container-test-all \
	container-setup-fcvm container-shell container-clean container-bench \
	setup-btrfs setup-fcvm setup-pjdfstest bench lint fmt ssh

all: build

help:
	@echo "fcvm Makefile"
	@echo ""
	@echo "Build:"
	@echo "  build              Build fcvm + fc-agent"
	@echo "  clean              Remove target directory"
	@echo ""
	@echo "Test (host):"
	@echo "  test-unit          Unit tests only (no VMs, no sudo)"
	@echo "  test-fast          + quick VM tests (rootless, no sudo)"
	@echo "  test-all           + slow VM tests (rootless, no sudo)"
	@echo "  test-root, test    + privileged tests (bridged, pjdfstest, sudo)"
	@echo ""
	@echo "Test (container):"
	@echo "  container-test-unit    Unit tests in container"
	@echo "  container-test-fast    + quick VM tests in container"
	@echo "  container-test-all, container-test  + slow VM tests in container"
	@echo ""
	@echo "Container:"
	@echo "  container-build    Build test container"
	@echo "  container-shell    Interactive shell in container"
	@echo "  container-bench    Run benchmarks in container"
	@echo ""
	@echo "Setup:"
	@echo "  setup-btrfs        Create btrfs loopback at /mnt/fcvm-btrfs"
	@echo "  setup-fcvm         Download kernel and create rootfs"
	@echo "  setup-pjdfstest    Build pjdfstest"
	@echo "  install-host-kernel  Build and install host kernel with patches (requires reboot)"
	@echo ""
	@echo "Other:"
	@echo "  bench              Run fuse-pipe benchmarks"
	@echo "  lint               Run linting (fmt, clippy, audit)"
	@echo "  fmt                Format code"
	@echo "  clean-test-data    Remove VM disks, snapshots, state (keeps cached assets)"
	@echo "  check-disk         Check disk space requirements"
	@echo ""
	@echo "Options: FILTER=pattern STREAM=1 LIST=1"

# Disk space check - fails if either root or btrfs is too full
# Requires 10GB free on root (for cargo target) and 15GB on btrfs (for VMs)
check-disk:
	@# Fix advisory-db ownership (sudo/non-sudo mixing corrupts it)
	@sudo chown -R $$(id -u):$$(id -g) "$$HOME/.cargo/advisory-db" 2>/dev/null || true
	@sudo chown -R $$(id -u):$$(id -g) "$$HOME/.cargo/advisory-dbs" 2>/dev/null || true
	@ROOT_FREE=$$(df -BG / 2>/dev/null | awk 'NR==2 {gsub("G",""); print $$4}'); \
	BTRFS_FREE=$$(df -BG /mnt/fcvm-btrfs 2>/dev/null | awk 'NR==2 {gsub("G",""); print $$4}'); \
	if [ -n "$$ROOT_FREE" ] && [ "$$ROOT_FREE" -lt 10 ]; then \
		echo "ERROR: Need 10GB free on / (have $${ROOT_FREE}GB)"; \
		echo "Try: make clean"; \
		exit 1; \
	fi; \
	if [ -n "$$BTRFS_FREE" ] && [ "$$BTRFS_FREE" -lt 15 ]; then \
		echo "ERROR: Need 15GB free on /mnt/fcvm-btrfs (have $${BTRFS_FREE}GB)"; \
		echo "Try: make clean-test-data"; \
		exit 1; \
	fi; \
	echo "Disk check passed: / has $${ROOT_FREE}GB, /mnt/fcvm-btrfs has $${BTRFS_FREE}GB"

# Clean leftover test data (VM disks, snapshots, state files)
# Preserves cached assets (kernels, rootfs, initrd, image-cache)
clean-test-data:
	@echo "==> Cleaning leftover VM disks..."
	sudo rm -rf /mnt/fcvm-btrfs/vm-disks/*
	sudo rm -rf $(ROOT_DATA_DIR)/vm-disks/* $(CONTAINER_DATA_DIR)/vm-disks/*
	@echo "==> Cleaning snapshots..."
	sudo rm -rf /mnt/fcvm-btrfs/snapshots/*
	sudo rm -rf $(ROOT_DATA_DIR)/snapshots/* $(CONTAINER_DATA_DIR)/snapshots/*
	@echo "==> Cleaning state files..."
	sudo rm -rf /mnt/fcvm-btrfs/state/*.json
	sudo rm -rf $(ROOT_DATA_DIR)/state/*.json $(CONTAINER_DATA_DIR)/state/*.json
	@echo "==> Cleaning UFFD sockets..."
	sudo rm -f /mnt/fcvm-btrfs/uffd-*.sock
	sudo rm -f $(ROOT_DATA_DIR)/uffd-*.sock $(CONTAINER_DATA_DIR)/uffd-*.sock
	@echo "==> Cleaning test logs..."
	rm -rf /tmp/fcvm-test-logs/*
	@echo "==> Cleaned test data (preserved cached assets)"

build:
	@echo "==> Building..."
	CARGO_TARGET_DIR=target cargo build --release -p fcvm
	CARGO_TARGET_DIR=target cargo build --release -p fc-agent --target $(MUSL_TARGET)
	@mkdir -p target/release && cp target/$(MUSL_TARGET)/release/fc-agent target/release/fc-agent
	@# Sync embedded config to user config dir (config is embedded at compile time)
	@./target/release/fcvm setup --generate-config --force 2>/dev/null || true

# Test that the release binary works without source tree (simulates cargo install)
test-packaging: build
	@echo "==> Testing packaging (simulates cargo install)..."
	./scripts/test-packaging.sh target/release/fcvm

clean:
	sudo rm -rf target

# Run-only targets (no setup deps, used by container)
_test-unit:
	$(NEXTEST) --no-default-features

_test-fast:
	RUST_LOG="$(TEST_LOG)" \
	$(NEXTEST) $(NEXTEST_CAPTURE) --no-default-features --features integration-fast $(FILTER)

_test-all:
	RUST_LOG="$(TEST_LOG)" \
	$(NEXTEST) $(NEXTEST_CAPTURE) $(FILTER)

_test-root:
	RUST_LOG="$(TEST_LOG)" \
	FCVM_DATA_DIR=$(ROOT_DATA_DIR) \
	CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER='sudo -E' \
	CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER='sudo -E' \
	$(NEXTEST) $(NEXTEST_CAPTURE) $(NEXTEST_IGNORED) --retries 2 --features privileged-tests $(FILTER)

# Host targets (with setup, check-disk first to fail fast if disk is full)
test-unit: show-notes check-disk build _test-unit
test-fast: show-notes check-disk setup-fcvm _test-fast
test-all: show-notes check-disk setup-fcvm _test-all
test-root: show-notes check-disk setup-fcvm setup-pjdfstest _test-root
test: test-root

# Container targets (setup on host where needed, run-only in container)
# Container uses shadowed target/ mount to avoid permission conflicts
# check-disk runs on host before container tests start
container-test-unit: check-disk container-build
	@echo "==> Running unit tests in container..."
	$(CONTAINER_RUN) $(CONTAINER_TAG) make build _test-unit

container-test-fast: check-disk container-setup-fcvm
	@echo "==> Running fast tests in container..."
	$(CONTAINER_RUN) $(CONTAINER_TAG) make _test-fast

container-test-all: check-disk container-setup-fcvm
	@echo "==> Running all tests in container..."
	$(CONTAINER_RUN) $(CONTAINER_TAG) make _test-all

container-test: container-test-all

CONTAINER_CACHE_REPO ?= ghcr.io/ejc3/fcvm-cache

container-build:
	@sudo mkdir -p /mnt/fcvm-btrfs 2>/dev/null || true
	@mkdir -p /tmp/fcvm-container-target
	podman build -t $(CONTAINER_TAG) -f Containerfile --build-arg ARCH=$(CONTAINER_ARCH) \
		--layers --cache-from $(CONTAINER_CACHE_REPO) --cache-to $(CONTAINER_CACHE_REPO) .

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
			sudo truncate -s 60G /var/fcvm-btrfs.img && sudo mkfs.btrfs /var/fcvm-btrfs.img; \
		fi && \
		sudo mkdir -p /mnt/fcvm-btrfs && \
		sudo mount -o loop /var/fcvm-btrfs.img /mnt/fcvm-btrfs && \
		sudo mkdir -p /mnt/fcvm-btrfs/{kernels,rootfs,initrd,state,snapshots,vm-disks,cache} && \
		sudo chown -R $$(id -un):$$(id -gn) /mnt/fcvm-btrfs && \
		echo '==> btrfs ready at /mnt/fcvm-btrfs'; \
	fi
	@# Create per-mode data directories with world-writable permissions
	@sudo mkdir -p $(ROOT_DATA_DIR)/{state,snapshots,vm-disks} && sudo chmod -R 777 $(ROOT_DATA_DIR)
	@sudo mkdir -p $(CONTAINER_DATA_DIR)/{state,snapshots,vm-disks} && sudo chmod -R 777 $(CONTAINER_DATA_DIR)

setup-fcvm: build setup-btrfs
	@FREE_GB=$$(df -BG /mnt/fcvm-btrfs 2>/dev/null | awk 'NR==2 {gsub("G",""); print $$4}'); \
	if [ -n "$$FREE_GB" ] && [ "$$FREE_GB" -lt 15 ]; then \
		echo "ERROR: Need 15GB on /mnt/fcvm-btrfs (have $${FREE_GB}GB)"; \
		exit 1; \
	fi
	@echo "==> Running fcvm setup..."
	./target/release/fcvm setup

# Build and install host kernel with all patches from kernel/patches/
# Requires reboot to activate the new kernel
install-host-kernel: build setup-btrfs
	sudo ./target/release/fcvm setup --kernel-profile nested --build-kernels --install-host-kernel

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
	./target/release/fcvm setup --kernel-profile nested

bench: build
	@echo "==> Running benchmarks..."
	sudo cargo bench -p fuse-pipe --bench throughput
	sudo cargo bench -p fuse-pipe --bench operations
	cargo bench -p fuse-pipe --bench protocol

# Container benchmark target (used by nightly CI)
container-bench: container-build
	@echo "==> Running benchmarks in container..."
	$(CONTAINER_RUN) $(CONTAINER_TAG) make build _bench

_bench:
	@echo "==> Running benchmarks..."
	cargo bench -p fuse-pipe --bench throughput
	cargo bench -p fuse-pipe --bench operations
	cargo bench -p fuse-pipe --bench protocol

lint:
	cargo test --test lint

fmt:
	cargo fmt

# SSH to jumpbox (IP from terraform: cd ~/src/aws && terraform output jumpbox_ssh_command)
JUMPBOX_IP := 54.193.62.221
ssh:
	ssh -i ~/.ssh/fcvm-ec2 ubuntu@$(JUMPBOX_IP)
