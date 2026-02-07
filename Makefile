SHELL := /bin/bash

# Find Rust toolchain bin directory and set PATH
# Prefer stable (has musl target), fall back to any toolchain
RUST_BIN := $(shell command -v cargo >/dev/null 2>&1 && dirname $$(command -v cargo) || \
	(test -x $(HOME)/.cargo/bin/cargo && echo $(HOME)/.cargo/bin) || \
	(ls -d $(HOME)/.rustup/toolchains/stable-*/bin 2>/dev/null | head -1) || \
	(ls -d $(HOME)/.rustup/toolchains/*/bin 2>/dev/null | head -1))
export PATH := $(RUST_BIN):$(PATH)
CARGO := cargo

# Custom slirp4netns with newer libslirp (for IPv6 DNS support on RHEL9/CentOS9)
# Build with: ./scripts/build-slirp4netns.sh
CUSTOM_DEPS_BIN := /mnt/fcvm-btrfs/deps/bin
ifneq ($(wildcard $(CUSTOM_DEPS_BIN)/slirp4netns),)
export PATH := $(CUSTOM_DEPS_BIN):$(PATH)
endif

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
CONTAINER_ARCH ?= $(shell uname -m)

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

# On IPv6-only hosts, auto-exclude bridged tests (they require IPv4 iptables)
# User can still explicitly run bridged tests with FILTER=bridged
ifeq ($(IPV6_ONLY),1)
ifndef FILTER
# No filter set: exclude bridged tests
NEXTEST_PARTITION := --partition hash:1/2
IPV6_FILTER := -E 'not test(/bridged/)'
else
# User set a filter: respect it (they know what they're doing)
IPV6_FILTER :=
endif
else
IPV6_FILTER :=
endif

# Disable retries when FILTER is set (debugging specific tests)
ifdef FILTER
NEXTEST_RETRIES :=
else
NEXTEST_RETRIES := --retries 2
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

# IPv6-only detection: If no IPv4 default route exists, bridged networking won't work
# (bridged uses IPv4 iptables DNAT). Auto-exclude bridged tests on IPv6-only hosts.
HAS_IPV4 := $(shell ip route show default 2>/dev/null | grep -q . && echo 1 || echo 0)
ifeq ($(HAS_IPV4),0)
IPV6_ONLY := 1
$(info Note: IPv6-only host detected - bridged tests will be skipped)
endif

# Base test command
export CARGO_TARGET_DIR := target
NEXTEST := $(CARGO) nextest $(NEXTEST_CMD) --release

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
	--device /dev/fuse -v /dev/kvm:/dev/kvm -v /dev/userfaultfd:/dev/userfaultfd \
	--ulimit nofile=65536:65536 --ulimit nproc=65536:65536 --pids-limit=65536 -v /mnt/fcvm-btrfs:/mnt/fcvm-btrfs \
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
	@echo "  setup-lint-tools   Install cargo-audit and cargo-deny"
	@echo "  install-host-kernel  Build and install host kernel with patches (requires reboot)"
	@echo ""
	@echo "Kernel patches:"
	@echo "  kernel-patch-create PROFILE=nested NAME=0004-fix FILE=fs/fuse/dir.c"
	@echo "  kernel-patch-edit PROFILE=nested PATCH=0002"
	@echo "  kernel-patch-validate PROFILE=nested"
	@echo ""
	@echo "Other:"
	@echo "  bench              Run fuse-pipe benchmarks"
	@echo "  lint               Run linting (auto-installs tools if needed)"
	@echo "  fmt                Format code"
	@echo "  clean-test-data    Remove VM disks, snapshots, state (keeps cached assets)"
	@echo "  check-disk         Check disk space requirements"
	@echo ""
	@echo "Options: FILTER=pattern STREAM=1 LIST=1"

# Disk space check - fails if either root or btrfs is too full
# Requires 10GB free on root (for cargo target) and 15GB on btrfs (for VMs)
check-disk:
	@# Ensure test log directory exists for container mounts
	@mkdir -p $(TEST_LOG_DIR)
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
# CRITICAL: Uses fcvm's proper cleanup commands to handle btrfs CoW correctly
clean-test-data: build
	@echo "==> Killing stale VM processes from previous runs..."
	@sudo pkill -9 firecracker 2>/dev/null; sudo pkill -9 slirp4netns 2>/dev/null; sleep 1; true
	@echo "==> Force unmounting stale FUSE mounts..."
	@# Find and force unmount any FUSE mounts from previous test runs
	@mount | grep fuse | grep -E '/tmp|/var/tmp' | cut -d' ' -f3 | xargs -r -I{} fusermount3 -u -z {} 2>/dev/null || true
	@echo "==> Cleaning snapshots via fcvm (handles btrfs CoW properly)..."
	@# Use fcvm's snapshot prune for proper cleanup - handles reflinks correctly
	sudo ./target/release/fcvm snapshots prune --all --force 2>/dev/null || true
	@# Also clean per-mode directories
	sudo FCVM_DATA_DIR=$(ROOT_DATA_DIR) ./target/release/fcvm snapshots prune --all --force 2>/dev/null || true
	sudo FCVM_DATA_DIR=$(CONTAINER_DATA_DIR) ./target/release/fcvm snapshots prune --all --force 2>/dev/null || true
	@echo "==> Cleaning leftover VM disks..."
	sudo rm -rf /mnt/fcvm-btrfs/vm-disks/*
	sudo rm -rf $(ROOT_DATA_DIR)/vm-disks/* $(CONTAINER_DATA_DIR)/vm-disks/*
	@echo "==> Cleaning state files..."
	sudo rm -rf /mnt/fcvm-btrfs/state/*.json /mnt/fcvm-btrfs/state/*.lock
	sudo rm -rf $(ROOT_DATA_DIR)/state/*.json $(ROOT_DATA_DIR)/state/*.lock
	sudo rm -rf $(CONTAINER_DATA_DIR)/state/*.json $(CONTAINER_DATA_DIR)/state/*.lock
	@echo "==> Cleaning UFFD sockets..."
	sudo rm -f /mnt/fcvm-btrfs/uffd-*.sock
	sudo rm -f $(ROOT_DATA_DIR)/uffd-*.sock $(CONTAINER_DATA_DIR)/uffd-*.sock
	@echo "==> Cleaning test logs..."
	rm -rf /tmp/fcvm-test-logs/*
	@echo "==> Cleaned test data (preserved cached assets)"

build:
	@echo "==> Building..."
	CARGO_TARGET_DIR=target $(CARGO) build --release -p fcvm
	CARGO_TARGET_DIR=target $(CARGO) build --release -p fc-agent --target $(MUSL_TARGET)
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
	./scripts/no-sudo.sh $(NEXTEST) $(NEXTEST_CAPTURE) $(NEXTEST_RETRIES) --no-default-features --features integration-fast $(FILTER)

_test-all:
	RUST_LOG="$(TEST_LOG)" \
	./scripts/no-sudo.sh $(NEXTEST) $(NEXTEST_CAPTURE) $(NEXTEST_RETRIES) $(FILTER)

_test-root:
	@RUST_LOG="$(TEST_LOG)" \
	FCVM_DATA_DIR=$(ROOT_DATA_DIR) \
	CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER='sudo -E env PATH=$(PATH)' \
	CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER='sudo -E env PATH=$(PATH)' \
	$(NEXTEST) $(NEXTEST_CAPTURE) $(NEXTEST_IGNORED) $(NEXTEST_RETRIES) --features privileged-tests $(IPV6_FILTER) $(FILTER) || \
	{ echo ""; \
	  echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"; \
	  echo "TEST FAILED - Check debug logs for root cause:"; \
	  echo "  📋 Debug logs: /tmp/fcvm-test-logs/*.log"; \
	  echo "  💡 Re-run with STREAM=1 to see tracing output in real-time"; \
	  echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"; \
	  exit 1; }

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
# Only push cache if authenticated to ghcr.io (CI has login, local dev doesn't)
CACHE_TO_FLAG := $(shell podman login --get-login ghcr.io >/dev/null 2>&1 && echo "--cache-to $(CONTAINER_CACHE_REPO)" || echo "")

container-build:
	@sudo mkdir -p /mnt/fcvm-btrfs 2>/dev/null || true
	@mkdir -p /tmp/fcvm-container-target
	podman build -t $(CONTAINER_TAG) -f Containerfile --build-arg ARCH=$(CONTAINER_ARCH) \
		--layers --cache-from $(CONTAINER_CACHE_REPO) $(CACHE_TO_FLAG) .

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
		sudo mkdir -p /mnt/fcvm-btrfs/{kernels,rootfs,initrd,cache,image-cache} && \
		sudo chown -R $$(id -un):$$(id -gn) /mnt/fcvm-btrfs && \
		echo '==> btrfs ready at /mnt/fcvm-btrfs'; \
	fi
	@# Ensure these dirs exist with correct permissions (may be missing after reboot/corruption)
	@sudo mkdir -p /mnt/fcvm-btrfs/image-cache /mnt/fcvm-btrfs/containers
	@sudo chown $$(id -un):$$(id -gn) /mnt/fcvm-btrfs/image-cache /mnt/fcvm-btrfs/containers
	@# Enable IP forwarding (required for bridged networking)
	@sudo sysctl -q -w net.ipv4.ip_forward=1
	@# Create per-mode data directories (state, snapshots, vm-disks)
	@# Default: owned by current user (test-fast runs as ubuntu)
	@mkdir -p /mnt/fcvm-btrfs/{state,snapshots,vm-disks}
	@# ROOT_DATA_DIR: owned by root (test-root runs with sudo)
	@sudo mkdir -p $(ROOT_DATA_DIR)/{state,snapshots,vm-disks}
	@# CONTAINER_DATA_DIR: owned by current user (podman rootless maps to subordinate UIDs)
	@sudo mkdir -p $(CONTAINER_DATA_DIR)/{state,snapshots,vm-disks}
	@sudo chown -R $$(id -un):$$(id -gn) $(CONTAINER_DATA_DIR)

setup-fcvm: build setup-btrfs
	@FREE_GB=$$(df -BG /mnt/fcvm-btrfs 2>/dev/null | awk 'NR==2 {gsub("G",""); print $$4}'); \
	if [ -n "$$FREE_GB" ] && [ "$$FREE_GB" -lt 15 ]; then \
		echo "ERROR: Need 15GB on /mnt/fcvm-btrfs (have $${FREE_GB}GB)"; \
		exit 1; \
	fi
	@echo "==> Running fcvm setup..."
	./target/release/fcvm setup
	@echo "==> Running fcvm setup --kernel-profile nested..."
	./target/release/fcvm setup --kernel-profile nested --build-kernels

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
	sudo ./target/release/fcvm setup --generate-config --force
	sudo ./target/release/fcvm setup
	sudo ./target/release/fcvm setup --kernel-profile nested --build-kernels

bench: build
	@echo "==> Running benchmarks..."
	sudo $(CARGO) bench -p fuse-pipe --bench throughput
	sudo $(CARGO) bench -p fuse-pipe --bench operations
	$(CARGO) bench -p fuse-pipe --bench protocol

# VM benchmarks (exec, clone) - require KVM, Firecracker, setup
bench-vm: build setup-fcvm
	@echo "==> Running VM benchmarks..."
	sudo $(CARGO) bench --bench exec -- --test
	sudo $(CARGO) bench --bench clone -- --test

# Container benchmark target (used by nightly CI)
container-bench: check-disk container-build
	@echo "==> Running benchmarks in container..."
	$(CONTAINER_RUN) $(CONTAINER_TAG) make build _bench

_bench:
	@echo "==> Running benchmarks..."
	$(CARGO) bench -p fuse-pipe --bench throughput
	$(CARGO) bench -p fuse-pipe --bench operations
	$(CARGO) bench -p fuse-pipe --bench protocol

# Lint tools versions (keep in sync with CI)
CARGO_AUDIT_VERSION := 0.22.0
CARGO_DENY_VERSION := 0.18.9

setup-lint-tools:
	@which cargo-audit > /dev/null || (echo "Installing cargo-audit..." && cargo install cargo-audit@$(CARGO_AUDIT_VERSION) --locked)
	@which cargo-deny > /dev/null || (echo "Installing cargo-deny..." && cargo install cargo-deny@$(CARGO_DENY_VERSION) --locked)

lint: setup-lint-tools
	$(CARGO) fmt -p fcvm -p fuse-pipe -p fc-agent --check
	$(CARGO) clippy --all-targets -- -D warnings
	$(CARGO) audit
	$(CARGO) deny check

fmt:
	$(CARGO) fmt

# SSH to jumpbox (IP from terraform: cd ~/src/aws && terraform output jumpbox_ssh_command)
JUMPBOX_IP := 54.193.62.221
ssh:
	ssh -i ~/.ssh/fcvm-ec2 ubuntu@$(JUMPBOX_IP)

# Kernel patch helpers - generates properly formatted patches
# Usage: make kernel-patch-create PROFILE=nested NAME=0004-my-fix FILE=fs/fuse/dir.c
PROFILE ?= nested
NAME ?=
PATCH ?=
FILE ?=

kernel-patch-create:
	@test -n "$(NAME)" || (echo "ERROR: NAME required (e.g., NAME=0004-my-fix)"; exit 1)
	@test -n "$(FILE)" || (echo "ERROR: FILE required (e.g., FILE=fs/fuse/dir.c)"; exit 1)
	./scripts/kernel-patch.sh create $(PROFILE) $(NAME) $(FILE)

kernel-patch-edit:
	@test -n "$(PATCH)" || (echo "ERROR: PATCH required (e.g., PATCH=0002)"; exit 1)
	./scripts/kernel-patch.sh edit $(PROFILE) $(PATCH)

kernel-patch-validate:
	./scripts/kernel-patch.sh validate $(PROFILE)
