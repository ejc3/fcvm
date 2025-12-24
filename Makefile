SHELL := /bin/bash

# Paths (can be overridden via environment for CI)
FUSE_BACKEND_RS ?= /home/ubuntu/fuse-backend-rs
FUSER ?= /home/ubuntu/fuser

# SUDO prefix - override to empty when already root (e.g., in container)
SUDO ?= sudo

# Separate target directories for sudo vs non-sudo builds
# This prevents permission conflicts when running tests in parallel
TARGET_DIR := target
TARGET_DIR_ROOT := target-root

# Container image name and architecture
CONTAINER_IMAGE := fcvm-test
CONTAINER_ARCH ?= aarch64

# Test filter - use to run subset of tests
# Usage: make test-vm FILTER=sanity    (runs only *sanity* tests)
#        make test-vm FILTER=exec      (runs only *exec* tests)
FILTER ?=

# Stream test output (disable capture) - use for debugging
# Usage: make test-vm STREAM=1         (show output as tests run)
STREAM ?= 0
ifeq ($(STREAM),1)
NEXTEST_CAPTURE := --no-capture
else
NEXTEST_CAPTURE :=
endif

# Enable fc-agent strace debugging - use to diagnose fc-agent crashes
# Usage: make test-vm STRACE=1         (runs fc-agent under strace in VM)
STRACE ?= 0
ifeq ($(STRACE),1)
FCVM_STRACE_AGENT := 1
else
FCVM_STRACE_AGENT :=
endif

# Test commands - organized by root requirement
# Uses cargo-nextest for better parallelism and output handling
# Host tests use CARGO_TARGET_DIR for sudo/non-sudo isolation
# Container tests don't need CARGO_TARGET_DIR - volume mounts provide isolation
#
# nextest benefits:
# - Each test runs in own process (better isolation)
# - Smart parallelism with test groups (see .config/nextest.toml)
# - No doctests by default (no --tests flag needed)
# - Better output: progress, timing, failures highlighted

# No root required (uses TARGET_DIR):
TEST_UNIT := CARGO_TARGET_DIR=$(TARGET_DIR) cargo nextest run --release --lib
TEST_FUSE_NOROOT := CARGO_TARGET_DIR=$(TARGET_DIR) cargo nextest run --release -p fuse-pipe --test integration
TEST_FUSE_STRESS := CARGO_TARGET_DIR=$(TARGET_DIR) cargo nextest run --release -p fuse-pipe --test test_mount_stress

# Root required (uses TARGET_DIR_ROOT):
TEST_FUSE_ROOT := CARGO_TARGET_DIR=$(TARGET_DIR_ROOT) cargo nextest run --release -p fuse-pipe --test integration_root
# Note: test_permission_edge_cases requires C pjdfstest with -u/-g flags, only available in container
TEST_PJDFSTEST := CARGO_TARGET_DIR=$(TARGET_DIR_ROOT) cargo nextest run --release -p fuse-pipe --test pjdfstest_full

# VM tests: privileged-tests feature gates tests that require sudo
# Unprivileged tests run by default (no feature flag)
# Use -p fcvm to only run fcvm package tests (excludes fuse-pipe)
#
# VM test command - runs all tests with privileged-tests feature
# Sets target runner to "sudo -E" so test binaries run with privileges
# (not set globally in .cargo/config.toml to avoid affecting non-root tests)
# Excludes rootless tests which have signal handling issues under sudo
TEST_VM := sh -c "CARGO_TARGET_DIR=$(TARGET_DIR) FCVM_STRACE_AGENT=$(FCVM_STRACE_AGENT) CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER='sudo -E' CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER='sudo -E' cargo nextest run -p fcvm --release $(NEXTEST_CAPTURE) --features privileged-tests -E '!test(/rootless/)' $(FILTER)"

# Container test commands (no CARGO_TARGET_DIR - volume mounts provide isolation)
# No global target runner in .cargo/config.toml, so these run without sudo by default
CTEST_UNIT := cargo nextest run --release --lib
CTEST_FUSE_NOROOT := cargo nextest run --release -p fuse-pipe --test integration
CTEST_FUSE_STRESS := cargo nextest run --release -p fuse-pipe --test test_mount_stress
CTEST_FUSE_ROOT := cargo nextest run --release -p fuse-pipe --test integration_root
CTEST_FUSE_PERMISSION := cargo nextest run --release -p fuse-pipe --test test_permission_edge_cases
CTEST_PJDFSTEST := cargo nextest run --release -p fuse-pipe --test pjdfstest_full

# Container VM tests now use `make test-vm-*` inside container (see container-test-vm-* targets)

# Benchmark commands (fuse-pipe)
BENCH_THROUGHPUT := cargo bench -p fuse-pipe --bench throughput
BENCH_OPERATIONS := cargo bench -p fuse-pipe --bench operations
BENCH_PROTOCOL := cargo bench -p fuse-pipe --bench protocol

# Benchmark commands (fcvm - requires VMs)
BENCH_EXEC := cargo bench --bench exec

.PHONY: all help build build-root build-all clean \
        test test-noroot test-root test-unit test-fuse test-vm test-all \
        test-pjdfstest test-all-host test-all-container ci-local pre-push \
        bench bench-throughput bench-operations bench-protocol bench-exec bench-quick bench-logs bench-clean \
        lint clippy fmt fmt-check \
        container-build container-build-root container-build-rootless container-build-only container-build-allow-other \
        container-test container-test-unit container-test-noroot container-test-root container-test-fuse \
        container-test-vm container-test-pjdfstest container-test-all container-test-allow-other \
        container-bench container-bench-throughput container-bench-operations container-bench-protocol container-bench-exec \
        container-shell container-clean \
        setup-btrfs setup-rootfs setup-all

all: build

help:
	@echo "fcvm Build System"
	@echo ""
	@echo "Development:"
	@echo "  make build       - Build fcvm and fc-agent"
	@echo "  make clean       - Clean build artifacts"
	@echo ""
	@echo "Testing (with optional FILTER and STREAM):"
	@echo "  VM tests run with sudo (via CARGO_TARGET_*_RUNNER env vars)"
	@echo "  Use FILTER= to filter tests matching a pattern, STREAM=1 for live output."
	@echo ""
	@echo "  make test-vm                    - All VM tests"
	@echo "  make test-vm FILTER=exec        - Only *exec* tests"
	@echo "  make test-vm FILTER=sanity      - Only *sanity* tests"
	@echo ""
	@echo "  make test            - All fuse-pipe tests"
	@echo "  make test-pjdfstest  - POSIX compliance (8789 tests)"
	@echo "  make test-all        - Everything"
	@echo ""
	@echo "Container Testing:"
	@echo "  make container-test-vm             - All VM tests"
	@echo "  make container-test-vm FILTER=exec - Only *exec* tests"
	@echo "  make container-test                - fuse-pipe tests"
	@echo "  make container-test-pjdfstest      - POSIX compliance"
	@echo "  make container-test-all            - Everything"
	@echo "  make container-shell               - Interactive shell"
	@echo ""
	@echo "Linting:"
	@echo "  make lint  - Run clippy + fmt-check"
	@echo "  make fmt   - Format code"
	@echo ""
	@echo "Setup:"
	@echo "  make setup-btrfs  - Create btrfs loopback (kernel/rootfs auto-created by fcvm)"

#------------------------------------------------------------------------------
# Setup targets (idempotent)
#------------------------------------------------------------------------------

# Create btrfs loopback filesystem if not mounted
# Kernel is auto-downloaded by fcvm binary from Kata release (see rootfs-plan.toml)
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

# Create base rootfs if missing (requires build + setup-btrfs)
# Rootfs and kernel are auto-created by fcvm binary on first VM start
setup-rootfs: build setup-btrfs
	@echo '==> Rootfs and kernel will be auto-created on first VM start'

# Full setup
setup-all: setup-btrfs setup-rootfs
	@echo "==> Setup complete"

#------------------------------------------------------------------------------
# Build targets
#------------------------------------------------------------------------------

# Detect musl target for current architecture
ARCH := $(shell uname -m)
ifeq ($(ARCH),aarch64)
MUSL_TARGET := aarch64-unknown-linux-musl
else ifeq ($(ARCH),x86_64)
MUSL_TARGET := x86_64-unknown-linux-musl
else
MUSL_TARGET := unknown
endif

# Build non-root targets (uses TARGET_DIR)
# Builds fcvm, fc-agent binaries AND test harnesses
# fc-agent is built with musl for static linking (portable across glibc versions)
build:
	@echo "==> Building non-root targets..."
	CARGO_TARGET_DIR=$(TARGET_DIR) cargo build --release -p fcvm
	@echo "==> Building fc-agent with musl (statically linked)..."
	CARGO_TARGET_DIR=$(TARGET_DIR) cargo build --release -p fc-agent --target $(MUSL_TARGET)
	@mkdir -p $(TARGET_DIR)/release
	cp $(TARGET_DIR)/$(MUSL_TARGET)/release/fc-agent $(TARGET_DIR)/release/fc-agent
	CARGO_TARGET_DIR=$(TARGET_DIR) cargo test --release --all-targets --no-run

# Build root targets (uses TARGET_DIR_ROOT, run with sudo)
# Builds fcvm, fc-agent binaries AND test harnesses
# fc-agent is built with musl for static linking (portable across glibc versions)
build-root:
	@echo "==> Building root targets..."
	sudo CARGO_TARGET_DIR=$(TARGET_DIR_ROOT) cargo build --release -p fcvm
	@echo "==> Building fc-agent with musl (statically linked)..."
	sudo CARGO_TARGET_DIR=$(TARGET_DIR_ROOT) cargo build --release -p fc-agent --target $(MUSL_TARGET)
	sudo mkdir -p $(TARGET_DIR_ROOT)/release
	sudo cp -f $(TARGET_DIR_ROOT)/$(MUSL_TARGET)/release/fc-agent $(TARGET_DIR_ROOT)/release/fc-agent
	sudo CARGO_TARGET_DIR=$(TARGET_DIR_ROOT) cargo test --release --all-targets --no-run

# Build everything (both target dirs)
build-all: build build-root

clean:
	# Use sudo to ensure we can remove any root-owned files
	sudo rm -rf $(TARGET_DIR) $(TARGET_DIR_ROOT)

#------------------------------------------------------------------------------
# Testing (native) - organized by root requirement
#------------------------------------------------------------------------------

# Tests that don't require root (run first for faster feedback)
test-noroot: build
	@echo "==> Running tests (no root required)..."
	$(TEST_UNIT)
	$(TEST_FUSE_NOROOT)
	$(TEST_FUSE_STRESS)

# Tests that require root
test-root: build-root
	@echo "==> Running tests (root required)..."
	sudo $(TEST_FUSE_ROOT)

# All fuse-pipe tests: noroot first, then root
test: test-noroot test-root

# Unit tests only
test-unit: build
	$(TEST_UNIT)

# All fuse-pipe tests (needs both builds)
test-fuse: build build-root
	$(TEST_FUSE_NOROOT)
	$(TEST_FUSE_STRESS)
	sudo $(TEST_FUSE_ROOT)

# VM tests - runs all tests with privileged-tests feature
# Test binaries run with sudo via CARGO_TARGET_*_RUNNER env vars
# Use FILTER= to run subset, e.g.: make test-vm FILTER=exec
test-vm: build setup-btrfs
ifeq ($(STREAM),1)
	@echo "==> STREAM=1: Output streams live (parallel disabled)"
else
	@echo "==> STREAM=0: Output captured until test completes (use STREAM=1 for live output)"
endif
	$(TEST_VM)

# POSIX compliance tests (host - requires pjdfstest installed)
test-pjdfstest: build-root
	@echo "==> Running POSIX compliance tests (8789 tests)..."
	sudo $(TEST_PJDFSTEST)

# Run everything (use container-test-pjdfstest for POSIX compliance)
test-all: test test-vm test-pjdfstest

#------------------------------------------------------------------------------
# Benchmarks (native)
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

bench-exec: build setup-btrfs
	@echo "==> Running exec benchmarks (bridged vs rootless)..."
	sudo $(BENCH_EXEC)

bench-quick: build
	@echo "==> Running quick benchmarks..."
	sudo cargo bench -p fuse-pipe --bench throughput -- --quick
	sudo cargo bench -p fuse-pipe --bench operations -- --quick

bench-logs:
	@echo "==> Recent benchmark logs..."
	@ls -lt /tmp/fuse-bench-*.log 2>/dev/null | head -5 || echo 'No logs found'
	@echo ""
	@echo "==> Latest telemetry..."
	@cat $$(ls -t /tmp/fuse-bench-telemetry-*.json 2>/dev/null | head -1) 2>/dev/null | jq . || echo 'No telemetry found'

bench-clean:
	@echo "==> Cleaning benchmark artifacts..."
	rm -rf target/criterion
	rm -f /tmp/fuse-bench-*.log /tmp/fuse-bench-telemetry-*.json /tmp/fuse-stress*.sock /tmp/fuse-ops-bench-*.sock

#------------------------------------------------------------------------------
# Linting
#------------------------------------------------------------------------------

lint: clippy fmt-check

clippy:
	@echo "==> Running clippy..."
	cargo clippy --all-targets --all-features -- -D warnings

fmt:
	@echo "==> Formatting code..."
	cargo fmt

fmt-check:
	@echo "==> Checking format..."
	cargo fmt -- --check


#------------------------------------------------------------------------------
# Container testing
#------------------------------------------------------------------------------

# Container tag - podman layer caching handles incremental builds
CONTAINER_TAG := fcvm-test:latest

# CI mode: use host directories instead of named volumes (for artifact sharing)
# Set CI=1 to enable artifact-compatible mode
# Note: Container tests use separate volumes for root vs non-root to avoid permission conflicts
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

# Container run with source mounts (code always fresh, can't run stale)
# Cargo cache goes to testuser's home so non-root builds work
# Note: We have separate bases for root vs non-root to use different target volumes
# Uses rootless podman - no sudo needed. --privileged grants capabilities within
# user namespace which is sufficient for fuse tests and VM tests.
CONTAINER_RUN_BASE := podman run --rm --privileged \
	--group-add keep-groups \
	-v .:/workspace/fcvm \
	-v $(FUSE_BACKEND_RS):/workspace/fuse-backend-rs \
	-v $(FUSER):/workspace/fuser \
	$(VOLUME_TARGET) \
	$(VOLUME_CARGO) \
	-e CARGO_HOME=/home/testuser/.cargo

# Same as CONTAINER_RUN_BASE but uses separate target volume for root tests
CONTAINER_RUN_BASE_ROOT := podman run --rm --privileged \
	--group-add keep-groups \
	-v .:/workspace/fcvm \
	-v $(FUSE_BACKEND_RS):/workspace/fuse-backend-rs \
	-v $(FUSER):/workspace/fuser \
	$(VOLUME_TARGET_ROOT) \
	$(VOLUME_CARGO) \
	-e CARGO_HOME=/home/testuser/.cargo

# Container run options for fuse-pipe tests (non-root)
CONTAINER_RUN_FUSE := $(CONTAINER_RUN_BASE) \
	--device /dev/fuse \
	--ulimit nofile=65536:65536 \
	--ulimit nproc=65536:65536 \
	--pids-limit=-1

# Container run options for fuse-pipe tests (root)
# Note: --device-cgroup-rule not supported in rootless mode
# Uses --user root to override Containerfile's USER testuser
CONTAINER_RUN_FUSE_ROOT := $(CONTAINER_RUN_BASE_ROOT) \
	--user root \
	--device /dev/fuse \
	--ulimit nofile=65536:65536 \
	--ulimit nproc=65536:65536 \
	--pids-limit=-1

# Container run options for fcvm tests (adds KVM, btrfs, netns)
# Used for bridged mode tests that require root/iptables
# REQUIRES sudo - network namespace creation needs real root, not user namespace root
# Uses VOLUME_TARGET_ROOT for isolation from rootless podman builds
# Note: /run/systemd/resolve mount provides real DNS servers when host uses systemd-resolved
CONTAINER_RUN_FCVM := sudo podman run --rm --privileged \
	--group-add keep-groups \
	-v .:/workspace/fcvm \
	-v $(FUSE_BACKEND_RS):/workspace/fuse-backend-rs \
	-v $(FUSER):/workspace/fuser \
	$(VOLUME_TARGET_ROOT) \
	$(VOLUME_CARGO) \
	-e CARGO_HOME=/home/testuser/.cargo \
	--device /dev/kvm \
	--device /dev/fuse \
	--ulimit nofile=65536:65536 \
	--ulimit nproc=65536:65536 \
	--pids-limit=-1 \
	-v /mnt/fcvm-btrfs:/mnt/fcvm-btrfs \
	-v /var/run/netns:/var/run/netns:rshared \
	-v /run/systemd/resolve:/run/systemd/resolve:ro \
	--network host

# Container run for rootless networking tests
# Uses rootless podman (no sudo!) with --privileged for user namespace capabilities.
# --privileged with rootless podman grants capabilities within the user namespace,
# not actual host root. We're root inside the container but unprivileged on host.
# --group-add keep-groups preserves host user's groups (kvm) for /dev/kvm access.
# --device /dev/userfaultfd needed for snapshot/clone UFFD memory sharing.
# The container's user namespace is the isolation boundary.
ifeq ($(CI),1)
VOLUME_TARGET_ROOTLESS := -v ./target:/workspace/fcvm/target
VOLUME_CARGO_ROOTLESS := -v ./cargo-home:/home/testuser/.cargo
else
VOLUME_TARGET_ROOTLESS := -v fcvm-cargo-target-rootless:/workspace/fcvm/target
VOLUME_CARGO_ROOTLESS := -v fcvm-cargo-home-rootless:/home/testuser/.cargo
endif
CONTAINER_RUN_ROOTLESS := podman --root=/tmp/podman-rootless run --rm \
	--privileged \
	--group-add keep-groups \
	-v .:/workspace/fcvm \
	-v $(FUSE_BACKEND_RS):/workspace/fuse-backend-rs \
	-v $(FUSER):/workspace/fuser \
	$(VOLUME_TARGET_ROOTLESS) \
	$(VOLUME_CARGO_ROOTLESS) \
	-e CARGO_HOME=/home/testuser/.cargo \
	--device /dev/kvm \
	--device /dev/net/tun \
	--device /dev/userfaultfd \
	-v /mnt/fcvm-btrfs:/mnt/fcvm-btrfs \
	--network host

# Build containers - podman layer caching handles incremental builds
# CONTAINER_ARCH can be overridden: export CONTAINER_ARCH=x86_64 for CI
container-build:
	@echo "==> Building rootless container (ARCH=$(CONTAINER_ARCH))..."
	podman build -t $(CONTAINER_TAG) -f Containerfile --build-arg ARCH=$(CONTAINER_ARCH) .

container-build-root:
	@echo "==> Building root container (ARCH=$(CONTAINER_ARCH))..."
	sudo podman build -t $(CONTAINER_TAG) -f Containerfile --build-arg ARCH=$(CONTAINER_ARCH) .

container-build-rootless: container-build

# Container tests - organized by root requirement
# Non-root tests run with --user testuser to verify they don't need root
# fcvm unit tests with network ops skip themselves when not root
# Uses CTEST_* commands (no CARGO_TARGET_DIR - volume mounts provide isolation)
container-test-unit: container-build
	@echo "==> Running unit tests as non-root user..."
	$(CONTAINER_RUN_FUSE) --user testuser $(CONTAINER_TAG) $(CTEST_UNIT)

container-test-noroot: container-build
	@echo "==> Running tests as non-root user..."
	$(CONTAINER_RUN_FUSE) --user testuser $(CONTAINER_TAG) $(CTEST_UNIT)
	$(CONTAINER_RUN_FUSE) --user testuser $(CONTAINER_TAG) $(CTEST_FUSE_NOROOT)
	$(CONTAINER_RUN_FUSE) --user testuser $(CONTAINER_TAG) $(CTEST_FUSE_STRESS)

# Root tests run as root inside container (uses separate volume)
container-test-root: container-build-root
	@echo "==> Running tests as root..."
	$(CONTAINER_RUN_FUSE_ROOT) $(CONTAINER_TAG) $(CTEST_FUSE_ROOT)
	$(CONTAINER_RUN_FUSE_ROOT) $(CONTAINER_TAG) $(CTEST_FUSE_PERMISSION)

# All fuse-pipe tests (explicit) - matches native test-fuse
# Note: Uses both volumes since it mixes root and non-root tests
container-test-fuse: container-build container-build-root
	@echo "==> Running all fuse-pipe tests..."
	$(CONTAINER_RUN_FUSE) --user testuser $(CONTAINER_TAG) $(CTEST_FUSE_NOROOT)
	$(CONTAINER_RUN_FUSE) --user testuser $(CONTAINER_TAG) $(CTEST_FUSE_STRESS)
	$(CONTAINER_RUN_FUSE_ROOT) $(CONTAINER_TAG) $(CTEST_FUSE_ROOT)
	$(CONTAINER_RUN_FUSE_ROOT) $(CONTAINER_TAG) $(CTEST_FUSE_PERMISSION)

# Test AllowOther with user_allow_other configured (non-root with config)
# Uses separate image with user_allow_other pre-configured
CONTAINER_IMAGE_ALLOW_OTHER := fcvm-test-allow-other

container-build-allow-other: container-build
	@echo "==> Building allow-other container..."
	podman build -t $(CONTAINER_IMAGE_ALLOW_OTHER) -f Containerfile.allow-other .

container-test-allow-other: container-build-allow-other
	@echo "==> Testing AllowOther with user_allow_other in fuse.conf..."
	$(CONTAINER_RUN_FUSE) --user testuser $(CONTAINER_IMAGE_ALLOW_OTHER) cargo test --release -p fuse-pipe --test test_allow_other -- --nocapture

# All fuse-pipe tests: noroot first, then root
container-test: container-test-noroot container-test-root

# VM tests in container
# Uses privileged container, test binaries run with sudo via CARGO_TARGET_*_RUNNER
# Use FILTER= to run subset, e.g.: make container-test-vm FILTER=exec
container-test-vm: container-build-root setup-btrfs
	$(CONTAINER_RUN_FCVM) $(CONTAINER_TAG) make test-vm TARGET_DIR=target FILTER=$(FILTER) STREAM=$(STREAM) STRACE=$(STRACE)

container-test-pjdfstest: container-build-root
	$(CONTAINER_RUN_FUSE_ROOT) $(CONTAINER_TAG) $(CTEST_PJDFSTEST)

# Run everything in container
container-test-all: container-test container-test-vm container-test-pjdfstest

# Container benchmarks - uses same commands as native benchmarks
container-bench: container-build
	@echo "==> Running all fuse-pipe benchmarks..."
	$(CONTAINER_RUN_FUSE) $(CONTAINER_TAG) $(BENCH_THROUGHPUT)
	$(CONTAINER_RUN_FUSE) $(CONTAINER_TAG) $(BENCH_OPERATIONS)
	$(CONTAINER_RUN_FUSE) $(CONTAINER_TAG) $(BENCH_PROTOCOL)

container-bench-throughput: container-build
	$(CONTAINER_RUN_FUSE) $(CONTAINER_TAG) $(BENCH_THROUGHPUT)

container-bench-operations: container-build
	$(CONTAINER_RUN_FUSE) $(CONTAINER_TAG) $(BENCH_OPERATIONS)

container-bench-protocol: container-build
	$(CONTAINER_RUN_FUSE) $(CONTAINER_TAG) $(BENCH_PROTOCOL)

# fcvm exec benchmarks - requires VMs (uses CONTAINER_RUN_FCVM)
container-bench-exec: container-build setup-btrfs
	@echo "==> Running exec benchmarks (bridged vs rootless)..."
	$(CONTAINER_RUN_FCVM) $(CONTAINER_TAG) $(BENCH_EXEC)

container-shell: container-build
	$(CONTAINER_RUN_FUSE) -it $(CONTAINER_TAG) bash

# Force container rebuild (removes images and volumes)
container-clean:
	podman rmi $(CONTAINER_TAG) 2>/dev/null || true
	sudo podman rmi $(CONTAINER_TAG) 2>/dev/null || true
	podman volume rm fcvm-cargo-target fcvm-cargo-target-root fcvm-cargo-home 2>/dev/null || true

#------------------------------------------------------------------------------
# CI Simulation (local)
#------------------------------------------------------------------------------

# Run full CI locally with max parallelism
# Phase 1: Build all 5 target directories in parallel (host x2, container x3)
# Phase 2: Run all tests in parallel (they use pre-built binaries)
ci-local:
	@echo "==> Phase 1: Building all targets in parallel..."
	$(MAKE) -j build build-root container-build container-build-root container-build-rootless
	@echo "==> Phase 2: Running all tests in parallel..."
	$(MAKE) -j \
		lint \
		test-unit \
		test-fuse \
		test-pjdfstest \
		test-vm \
		container-test-noroot \
		container-test-root \
		container-test-pjdfstest \
		container-test-vm
	@echo "==> CI local complete"

# Quick pre-push check (just lint + unit, parallel)
pre-push: build
	$(MAKE) -j lint test-unit
	@echo "==> Ready to push"

# Host-only tests (parallel, builds both target dirs first)
# test-vm runs all VM tests (privileged + unprivileged)
test-all-host:
	$(MAKE) -j build build-root
	$(MAKE) -j lint test-unit test-fuse test-pjdfstest test-vm

# Container-only tests (parallel, builds all 3 container target dirs first)
test-all-container:
	$(MAKE) -j container-build container-build-root container-build-rootless
	$(MAKE) -j container-test-noroot container-test-root container-test-pjdfstest container-test-vm
