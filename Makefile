SHELL := /bin/bash

# Paths (can be overridden via environment for CI)
FUSE_BACKEND_RS ?= /home/ubuntu/fuse-backend-rs
FUSER ?= /home/ubuntu/fuser

# Target directory
TARGET_DIR := target

# Container image and architecture
CONTAINER_TAG := fcvm-test:latest
CONTAINER_ARCH ?= aarch64

# Test filter and options
FILTER ?=
STREAM ?= 0
STRACE ?= 0

ifeq ($(STREAM),1)
NEXTEST_CAPTURE := --no-capture
else
NEXTEST_CAPTURE :=
endif

ifeq ($(STRACE),1)
FCVM_STRACE_AGENT := 1
else
FCVM_STRACE_AGENT :=
endif

#------------------------------------------------------------------------------
# Test commands - use features to gate test tiers
#------------------------------------------------------------------------------

# Common environment for tests requiring sudo
TEST_ENV := CARGO_TARGET_DIR=$(TARGET_DIR) \
	FCVM_STRACE_AGENT=$(FCVM_STRACE_AGENT) \
	CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER='sudo -E' \
	CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER='sudo -E'

# Unit tests = fast, no VMs, no sudo (< 1s each)
# --no-default-features disables integration-fast and integration-slow
# Unit tests (cli parsing, state manager, health monitor, lint) always compile
TEST_UNIT := CARGO_TARGET_DIR=$(TARGET_DIR) cargo nextest run --release --no-default-features

# Integration-fast = unit + quick VM tests (< 30s each, requires sudo + KVM)
TEST_INTEGRATION_FAST := $(TEST_ENV) cargo nextest run --release $(NEXTEST_CAPTURE) \
	--no-default-features --features integration-fast,privileged-tests $(FILTER)

# Root = all tests (default features = all tiers, requires sudo + KVM)
TEST_ROOT := $(TEST_ENV) cargo nextest run --release $(NEXTEST_CAPTURE) \
	--features privileged-tests $(FILTER)

# Container test commands (call back to Makefile for single source of truth)
CTEST_UNIT := make test-unit
CTEST_INTEGRATION_FAST := make test-integration-fast
CTEST_ROOT := make test-root

# Benchmarks
BENCH_THROUGHPUT := cargo bench -p fuse-pipe --bench throughput
BENCH_OPERATIONS := cargo bench -p fuse-pipe --bench operations
BENCH_PROTOCOL := cargo bench -p fuse-pipe --bench protocol
BENCH_EXEC := cargo bench --bench exec

.PHONY: all help build clean \
	test test-unit test-integration-fast test-root \
	bench bench-throughput bench-operations bench-protocol bench-exec bench-quick bench-logs bench-clean \
	lint fmt \
	container-build container-build-root \
	container-test container-test-unit container-test-integration-fast container-test-root container-shell container-clean \
	ci-host ci-unit ci-integration-fast ci-root setup-btrfs setup-fcvm setup-pjdfstest

all: build

help:
	@echo "fcvm Build System"
	@echo ""
	@echo "Development:"
	@echo "  make build  - Build fcvm and fc-agent"
	@echo "  make clean  - Clean build artifacts"
	@echo "  make lint   - Run clippy + fmt-check"
	@echo ""
	@echo "Testing (host):"
	@echo "  make test                  - All tests (unit + integration + slow)"
	@echo "  make test-unit             - Unit tests only (no VMs, <1s each)"
	@echo "  make test-integration-fast - Quick tests (unit + VM tests <30s each)"
	@echo "  make test-root             - All tests (requires sudo + KVM)"
	@echo "  Options: FILTER=pattern STREAM=1"
	@echo ""
	@echo "Testing (container):"
	@echo "  make container-test                  - All tests"
	@echo "  make container-test-unit             - Unit tests only"
	@echo "  make container-test-integration-fast - Quick tests"
	@echo "  make container-test-root             - All tests"
	@echo "  make container-shell                 - Interactive shell"
	@echo ""
	@echo "Setup:"
	@echo "  make setup-btrfs  - Create btrfs loopback"
	@echo "  make setup-fcvm   - Download kernel and create rootfs"

#------------------------------------------------------------------------------
# Setup
#------------------------------------------------------------------------------

# pjdfstest POSIX compliance suite (built once, shared between host and container)
PJDFSTEST_DIR := /tmp/pjdfstest-check
PJDFSTEST_BIN := $(PJDFSTEST_DIR)/pjdfstest

setup-pjdfstest:
	@if [ ! -x $(PJDFSTEST_BIN) ]; then \
		echo '==> Building pjdfstest...'; \
		rm -rf $(PJDFSTEST_DIR) && \
		git clone --depth 1 https://github.com/pjd/pjdfstest $(PJDFSTEST_DIR) && \
		cd $(PJDFSTEST_DIR) && autoreconf -ifs && ./configure && make; \
	fi

setup-btrfs:
	@if ! mountpoint -q /mnt/fcvm-btrfs 2>/dev/null; then \
		echo '==> Creating btrfs loopback...'; \
		if [ ! -f /var/fcvm-btrfs.img ]; then \
			sudo truncate -s 20G /var/fcvm-btrfs.img && \
			sudo mkfs.btrfs /var/fcvm-btrfs.img; \
		fi && \
		sudo mkdir -p /mnt/fcvm-btrfs && \
		sudo mount -o loop /var/fcvm-btrfs.img /mnt/fcvm-btrfs && \
		sudo mkdir -p /mnt/fcvm-btrfs/{kernels,rootfs,initrd,state,snapshots,vm-disks,cache} && \
		sudo chown -R $$(id -un):$$(id -gn) /mnt/fcvm-btrfs && \
		echo '==> btrfs ready at /mnt/fcvm-btrfs'; \
	fi

# Download kernel and create rootfs (runs fcvm setup)
# Requires ~15GB free space on /mnt/fcvm-btrfs
setup-fcvm: build setup-btrfs
	@FREE_GB=$$(df -BG /mnt/fcvm-btrfs 2>/dev/null | awk 'NR==2 {gsub("G",""); print $$4}'); \
	if [ -n "$$FREE_GB" ] && [ "$$FREE_GB" -lt 15 ]; then \
		echo "ERROR: Not enough disk space on /mnt/fcvm-btrfs"; \
		echo "  Available: $${FREE_GB}GB, Required: 15GB"; \
		echo ""; \
		echo "Note: fcvm uses btrfs CoW (copy-on-write) for efficient disk usage."; \
		echo "      Each VM uses reflinks to share the base rootfs (~10GB)."; \
		echo ""; \
		echo "Remediation options:"; \
		echo "  1. Expand the btrfs image:"; \
		echo "     sudo truncate -s +10G /var/fcvm-btrfs.img"; \
		echo "     sudo btrfs filesystem resize max /mnt/fcvm-btrfs"; \
		echo ""; \
		echo "  2. Clean up old VM data (CoW reflinks):"; \
		echo "     sudo rm -rf /mnt/fcvm-btrfs/vm-disks/*"; \
		echo "     sudo rm -rf /mnt/fcvm-btrfs/cache/*"; \
		echo "     sudo rm -rf /mnt/fcvm-btrfs/snapshots/*"; \
		echo ""; \
		echo "  3. Clean up podman storage (container images inside VMs):"; \
		echo "     # These are stored in the rootfs, regenerate if needed"; \
		echo "     sudo rm -f /mnt/fcvm-btrfs/rootfs/layer2-*.raw"; \
		echo "     sudo rm -f /mnt/fcvm-btrfs/initrd/fc-agent-*.initrd"; \
		echo ""; \
		echo "  4. Delete and recreate everything:"; \
		echo "     sudo umount /mnt/fcvm-btrfs"; \
		echo "     sudo rm /var/fcvm-btrfs.img"; \
		echo "     make setup-btrfs"; \
		exit 1; \
	fi
	@echo "==> Running fcvm setup..."
	./$(TARGET_DIR)/release/fcvm setup

#------------------------------------------------------------------------------
# Build
#------------------------------------------------------------------------------

ARCH := $(shell uname -m)
ifeq ($(ARCH),aarch64)
MUSL_TARGET := aarch64-unknown-linux-musl
else ifeq ($(ARCH),x86_64)
MUSL_TARGET := x86_64-unknown-linux-musl
else
MUSL_TARGET := unknown
endif

build:
	@echo "==> Building..."
	CARGO_TARGET_DIR=$(TARGET_DIR) cargo build --release -p fcvm
	CARGO_TARGET_DIR=$(TARGET_DIR) cargo build --release -p fc-agent --target $(MUSL_TARGET)
	@mkdir -p $(TARGET_DIR)/release
	cp $(TARGET_DIR)/$(MUSL_TARGET)/release/fc-agent $(TARGET_DIR)/release/fc-agent
	CARGO_TARGET_DIR=$(TARGET_DIR) cargo test --release --all-targets --no-run

clean:
	sudo rm -rf $(TARGET_DIR) target-root

#------------------------------------------------------------------------------
# Testing (host)
#------------------------------------------------------------------------------

# Unit tests only (no VMs, no sudo)
test-unit: build
	@echo "==> Running unit tests..."
	$(TEST_UNIT)

# Integration-fast = unit + quick VM tests (requires sudo + KVM)
test-integration-fast: setup-fcvm
	@echo "==> Running integration-fast tests..."
	$(TEST_INTEGRATION_FAST)

# Root = all tests including slow ones (requires sudo + KVM)
test-root: setup-fcvm setup-pjdfstest
	@echo "==> Running all tests..."
	$(TEST_ROOT)

# All tests
test: test-root

#------------------------------------------------------------------------------
# Benchmarks
#------------------------------------------------------------------------------

bench: build
	@echo "==> Running all benchmarks..."
	sudo $(BENCH_THROUGHPUT)
	sudo $(BENCH_OPERATIONS)
	$(BENCH_PROTOCOL)

bench-throughput: build
	sudo $(BENCH_THROUGHPUT)

bench-operations: build
	sudo $(BENCH_OPERATIONS)

bench-protocol: build
	$(BENCH_PROTOCOL)

bench-exec: setup-fcvm
	sudo $(BENCH_EXEC)

bench-quick: build
	sudo cargo bench -p fuse-pipe --bench throughput -- --quick
	sudo cargo bench -p fuse-pipe --bench operations -- --quick

bench-logs:
	@ls -lt /tmp/fuse-bench-*.log 2>/dev/null | head -5 || echo 'No logs found'
	@cat $$(ls -t /tmp/fuse-bench-telemetry-*.json 2>/dev/null | head -1) 2>/dev/null | jq . || echo 'No telemetry found'

bench-clean:
	rm -rf target/criterion
	rm -f /tmp/fuse-bench-*.log /tmp/fuse-bench-telemetry-*.json /tmp/fuse-stress*.sock /tmp/fuse-ops-bench-*.sock

#------------------------------------------------------------------------------
# Linting (runs as tests for parallel execution)
#------------------------------------------------------------------------------

lint:
	cargo test --test lint

fmt:
	cargo fmt

#------------------------------------------------------------------------------
# Container
#------------------------------------------------------------------------------

# CI mode: use host directories instead of named volumes
CI ?= 0
ifeq ($(CI),1)
VOLUME_TARGET := -v ./target:/workspace/fcvm/target
VOLUME_TARGET_ROOT := -v ./target-root:/workspace/fcvm/target
VOLUME_CARGO := -v ./cargo-home:/home/testuser/.cargo
else
VOLUME_TARGET := -v fcvm-cargo-target:/workspace/fcvm/target
VOLUME_TARGET_ROOT := -v fcvm-cargo-target-root:/workspace/fcvm/target
VOLUME_CARGO := -v fcvm-cargo-home:/home/testuser/.cargo
endif

# Cache registry for CI layer caching
CACHE_REGISTRY ?=
CACHE_FLAGS := $(if $(CACHE_REGISTRY),--cache-from=$(CACHE_REGISTRY) --cache-to=$(CACHE_REGISTRY),)

# Container for rootless tests
CONTAINER_RUN_ROOTLESS := podman run --rm --privileged \
	--group-add keep-groups \
	-v .:/workspace/fcvm \
	-v $(FUSE_BACKEND_RS):/workspace/fuse-backend-rs \
	-v $(FUSER):/workspace/fuser \
	$(VOLUME_TARGET) \
	$(VOLUME_CARGO) \
	-e CARGO_HOME=/home/testuser/.cargo \
	--device /dev/fuse \
	--ulimit nofile=65536:65536

# Container for root tests (fuse + vm)
CONTAINER_RUN_ROOT := sudo podman run --rm --privileged \
	--group-add keep-groups \
	-v .:/workspace/fcvm \
	-v $(FUSE_BACKEND_RS):/workspace/fuse-backend-rs \
	-v $(FUSER):/workspace/fuser \
	$(VOLUME_TARGET_ROOT) \
	$(VOLUME_CARGO) \
	-e CARGO_HOME=/home/testuser/.cargo \
	--user root \
	--device /dev/fuse \
	--device /dev/kvm \
	--ulimit nofile=65536:65536 \
	-v /mnt/fcvm-btrfs:/mnt/fcvm-btrfs \
	-v /var/run/netns:/var/run/netns:rshared \
	-v /run/systemd/resolve:/run/systemd/resolve:ro \
	--network host

container-build:
	podman build -t $(CONTAINER_TAG) -f Containerfile --build-arg ARCH=$(CONTAINER_ARCH) $(CACHE_FLAGS) .

container-build-root:
	sudo podman build -t $(CONTAINER_TAG) -f Containerfile --build-arg ARCH=$(CONTAINER_ARCH) $(CACHE_FLAGS) .

# Unit tests only (in container)
container-test-unit: container-build
	@echo "==> Running unit tests in container..."
	$(CONTAINER_RUN_ROOTLESS) --user testuser $(CONTAINER_TAG) $(CTEST_UNIT)

# Integration-fast tests (in container)
container-test-integration-fast: container-build-root setup-fcvm
	@echo "==> Running integration-fast tests in container..."
	$(CONTAINER_RUN_ROOT) $(CONTAINER_TAG) $(CTEST_INTEGRATION_FAST)

# Root tests only (in container)
container-test-root: container-build-root setup-fcvm
	@echo "==> Running all tests in container..."
	$(CONTAINER_RUN_ROOT) $(CONTAINER_TAG) $(CTEST_ROOT)

# All tests
container-test: container-test-root

container-shell: container-build
	$(CONTAINER_RUN_ROOTLESS) -it $(CONTAINER_TAG) bash

container-clean:
	podman rmi $(CONTAINER_TAG) 2>/dev/null || true
	sudo podman rmi $(CONTAINER_TAG) 2>/dev/null || true
	podman volume rm fcvm-cargo-target fcvm-cargo-target-root fcvm-cargo-home 2>/dev/null || true

# CI targets (called by GitHub Actions)
ci-host: setup-fcvm
	$(MAKE) lint
	$(MAKE) test-root

ci-unit: container-build
	$(CONTAINER_RUN_ROOTLESS) --user testuser $(CONTAINER_TAG) $(CTEST_UNIT)

ci-integration-fast: container-build-root setup-fcvm
	$(CONTAINER_RUN_ROOT) $(CONTAINER_TAG) $(CTEST_INTEGRATION_FAST)

ci-root: container-build-root setup-fcvm
	$(CONTAINER_RUN_ROOT) $(CONTAINER_TAG) $(CTEST_ROOT)
