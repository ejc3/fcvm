SHELL := /bin/bash

# Paths (can be overridden via environment)
FUSE_BACKEND_RS ?= /home/ubuntu/fuse-backend-rs
FUSER ?= /home/ubuntu/fuser

# Container settings
CONTAINER_TAG := fcvm-test:latest
CONTAINER_ARCH ?= aarch64

# Per-mode data directories (prevents UID conflicts between test modes)
ROOT_DATA_DIR := /mnt/fcvm-btrfs/root
CONTAINER_DATA_DIR := /mnt/fcvm-btrfs/container

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
CONTAINER_RUN := podman run --rm --privileged \
	-v .:/workspace/fcvm \
	$(TARGET_MOUNT) \
	-v $(FUSE_BACKEND_RS):/workspace/fuse-backend-rs -v $(FUSER):/workspace/fuser \
	--device /dev/fuse --device /dev/kvm \
	--ulimit nofile=65536:65536 --pids-limit=65536 -v /mnt/fcvm-btrfs:/mnt/fcvm-btrfs \
	-v $(TEST_LOG_DIR):$(TEST_LOG_DIR) $(CARGO_CACHE_MOUNT) \
	-e FCVM_DATA_DIR=$(CONTAINER_DATA_DIR)

.PHONY: all help build clean clean-target clean-test-data check-disk \
	test test-unit test-fast test-all test-root \
	_test-unit _test-fast _test-all _test-root \
	container-build container-test container-test-unit container-test-fast container-test-all \
	container-shell container-clean setup-btrfs setup-fcvm setup-pjdfstest setup-inception bench lint fmt \
	rebuild-fc dev-fc-test inception-vm inception-exec inception-wait-exec inception-stop inception-status

all: build

help:
	@echo "fcvm: make build | test-unit | test-fast | test-all | test-root"
	@echo "      make container-test-unit | container-test-fast | container-test-all"
	@echo "      make clean-target | clean-test-data | check-disk"
	@echo "Options: FILTER=pattern STREAM=1 LIST=1"

# Disk space check - fails if either root or btrfs is too full
# Requires 10GB free on root (for cargo target) and 15GB on btrfs (for VMs)
check-disk:
	@ROOT_FREE=$$(df -BG / 2>/dev/null | awk 'NR==2 {gsub("G",""); print $$4}'); \
	BTRFS_FREE=$$(df -BG /mnt/fcvm-btrfs 2>/dev/null | awk 'NR==2 {gsub("G",""); print $$4}'); \
	if [ -n "$$ROOT_FREE" ] && [ "$$ROOT_FREE" -lt 10 ]; then \
		echo "ERROR: Need 10GB free on / (have $${ROOT_FREE}GB)"; \
		echo "Try: make clean-target"; \
		exit 1; \
	fi; \
	if [ -n "$$BTRFS_FREE" ] && [ "$$BTRFS_FREE" -lt 15 ]; then \
		echo "ERROR: Need 15GB free on /mnt/fcvm-btrfs (have $${BTRFS_FREE}GB)"; \
		echo "Try: make clean-test-data"; \
		exit 1; \
	fi; \
	echo "Disk check passed: / has $${ROOT_FREE}GB, /mnt/fcvm-btrfs has $${BTRFS_FREE}GB"

# Clean target directory (frees space on /)
clean-target:
	@echo "==> Cleaning target directory..."
	rm -rf target
	@echo "==> Cleaned target/"

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
	$(NEXTEST) $(NEXTEST_CAPTURE) --no-default-features --features integration-fast $(FILTER)

_test-all:
	$(NEXTEST) $(NEXTEST_CAPTURE) $(FILTER)

_test-root:
	FCVM_DATA_DIR=$(ROOT_DATA_DIR) \
	CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER='sudo -E' \
	CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER='sudo -E' \
	$(NEXTEST) $(NEXTEST_CAPTURE) --features privileged-tests $(FILTER)

# Host targets (with setup, check-disk first to fail fast if disk is full)
test-unit: check-disk build _test-unit
test-fast: check-disk setup-fcvm _test-fast
test-all: check-disk setup-fcvm _test-all
test-root: check-disk setup-fcvm setup-pjdfstest _test-root
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

container-build:
	@sudo mkdir -p /mnt/fcvm-btrfs 2>/dev/null || true
	@mkdir -p /tmp/fcvm-container-target
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

# Inception test setup - builds container with matching CAS chain
# Ensures: artifacts/fc-agent == target/release/fc-agent, initrd SHA matches, container cached
setup-inception: setup-fcvm
	@echo "==> Setting up inception test container..."
	@echo "==> Copying binaries to artifacts/..."
	mkdir -p artifacts
	cp target/release/fcvm artifacts/
	cp target/$(MUSL_TARGET)/release/fc-agent artifacts/
	cp /usr/local/bin/firecracker artifacts/firecracker-nv2 2>/dev/null || true
	@echo "==> Building inception-test container..."
	podman rmi localhost/inception-test 2>/dev/null || true
	podman build -t localhost/inception-test -f Containerfile.inception .
	@echo "==> Exporting container to CAS cache..."
	@DIGEST=$$(podman inspect localhost/inception-test --format '{{.Digest}}'); \
	CACHE_DIR="/mnt/fcvm-btrfs/image-cache/$${DIGEST}"; \
	if [ -d "$$CACHE_DIR" ]; then \
		echo "Cache already exists: $$CACHE_DIR"; \
	else \
		echo "Creating cache: $$CACHE_DIR"; \
		sudo mkdir -p "$$CACHE_DIR"; \
		sudo skopeo copy containers-storage:localhost/inception-test "dir:$$CACHE_DIR"; \
	fi
	@echo "==> Verification..."
	@echo "fc-agent SHA: $$(sha256sum artifacts/fc-agent | cut -c1-12)"
	@echo "Container fc-agent SHA: $$(podman run --rm localhost/inception-test sha256sum /usr/local/bin/fc-agent | cut -c1-12)"
	@echo "Initrd: $$(ls -1 /mnt/fcvm-btrfs/initrd/fc-agent-*.initrd | tail -1)"
	@DIGEST=$$(podman inspect localhost/inception-test --format '{{.Digest}}'); \
	echo "Image digest: $$DIGEST"; \
	echo "Cache path: /mnt/fcvm-btrfs/image-cache/$$DIGEST"
	@echo "==> Inception setup complete!"

bench: build
	@echo "==> Running benchmarks..."
	sudo cargo bench -p fuse-pipe --bench throughput
	sudo cargo bench -p fuse-pipe --bench operations
	cargo bench -p fuse-pipe --bench protocol

lint:
	cargo test --test lint

fmt:
	cargo fmt

# Firecracker development targets
# Rebuild Firecracker from source and install to /usr/local/bin
# Usage: make rebuild-fc
FIRECRACKER_SRC ?= /home/ubuntu/firecracker
FIRECRACKER_BIN := $(FIRECRACKER_SRC)/build/cargo_target/release/firecracker

rebuild-fc:
	@echo "==> Force rebuilding Firecracker..."
	touch $(FIRECRACKER_SRC)/src/vmm/src/arch/aarch64/vcpu.rs
	cd $(FIRECRACKER_SRC) && cargo build --release
	@echo "==> Installing Firecracker to /usr/local/bin..."
	sudo rm -f /usr/local/bin/firecracker
	sudo cp $(FIRECRACKER_BIN) /usr/local/bin/firecracker
	@echo "==> Verifying installation..."
	@strings /usr/local/bin/firecracker | grep -q "NV2 DEBUG" && echo "NV2 debug strings: OK" || echo "WARNING: NV2 debug strings missing"
	/usr/local/bin/firecracker --version

# Full rebuild cycle: Firecracker + fcvm + run test
# Usage: make dev-fc-test FILTER=inception
dev-fc-test: rebuild-fc build
	@echo "==> Running test with FILTER=$(FILTER)..."
	FCVM_DATA_DIR=$(ROOT_DATA_DIR) \
	CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER='sudo -E' \
	CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER='sudo -E' \
	RUST_LOG=debug \
	$(NEXTEST) $(NEXTEST_CAPTURE) --features privileged-tests $(FILTER)

# =============================================================================
# Inception VM development targets
# =============================================================================
# These targets manage a SINGLE inception VM for debugging.
# Only ONE VM can exist at a time - inception-vm kills any existing VM first.

# Find the inception kernel (latest vmlinux-*.bin with KVM support)
INCEPTION_KERNEL := $(shell ls -t /mnt/fcvm-btrfs/kernels/vmlinux-*.bin 2>/dev/null | head -1)
INCEPTION_VM_NAME := inception-dev
INCEPTION_VM_LOG := /tmp/inception-vm.log
INCEPTION_VM_PID := /tmp/inception-vm.pid

# Start an inception VM (kills any existing VM first)
# Usage: make inception-vm
inception-vm: build
	@echo "==> Ensuring clean environment (killing ALL existing VMs)..."
	@sudo pkill -9 firecracker 2>/dev/null || true
	@sudo pkill -9 -f "fcvm podman" 2>/dev/null || true
	@sleep 2
	@if pgrep firecracker >/dev/null 2>&1; then \
		echo "ERROR: Could not kill existing firecracker"; \
		exit 1; \
	fi
	@sudo rm -f $(INCEPTION_VM_PID) $(INCEPTION_VM_LOG)
	@sudo rm -rf /mnt/fcvm-btrfs/state/vm-*.json
	@if [ -z "$(INCEPTION_KERNEL)" ]; then \
		echo "ERROR: No inception kernel found. Run ./kernel/build.sh first."; \
		exit 1; \
	fi
	@echo "==> Starting SINGLE inception VM"
	@echo "==> Kernel: $(INCEPTION_KERNEL)"
	@echo "==> Log: $(INCEPTION_VM_LOG)"
	@echo "==> Use 'make inception-exec CMD=...' to run commands"
	@echo "==> Use 'make inception-stop' to stop"
	@sudo ./target/release/fcvm podman run \
		--name $(INCEPTION_VM_NAME) \
		--network bridged \
		--kernel $(INCEPTION_KERNEL) \
		--privileged \
		--map /mnt/fcvm-btrfs:/mnt/fcvm-btrfs \
		--cmd "sleep infinity" \
		alpine:latest > $(INCEPTION_VM_LOG) 2>&1 & \
	sleep 2; \
	FCVM_PID=$$(pgrep -n -f "fcvm podman run.*$(INCEPTION_VM_NAME)"); \
	echo "$$FCVM_PID" | sudo tee $(INCEPTION_VM_PID) > /dev/null; \
	echo "==> VM started with fcvm PID $$FCVM_PID"; \
	echo "==> Waiting for boot..."; \
	sleep 20; \
	FC_COUNT=$$(pgrep -c firecracker || echo 0); \
	if [ "$$FC_COUNT" -ne 1 ]; then \
		echo "ERROR: Expected 1 firecracker, got $$FC_COUNT"; \
		exit 1; \
	fi; \
	echo "==> VM ready. Tailing log (Ctrl+C to stop tail, VM keeps running):"; \
	tail -f $(INCEPTION_VM_LOG)

# Run a command inside the running inception VM
# Usage: make inception-exec CMD="ls -la /dev/kvm"
# Usage: make inception-exec CMD="/mnt/fcvm-btrfs/check_kvm_caps"
CMD ?= uname -a
inception-exec:
	@if [ ! -f $(INCEPTION_VM_PID) ]; then \
		echo "ERROR: No PID file found at $(INCEPTION_VM_PID)"; \
		echo "Start a VM with 'make inception-vm' first."; \
		exit 1; \
	fi; \
	PID=$$(cat $(INCEPTION_VM_PID)); \
	if ! kill -0 $$PID 2>/dev/null; then \
		echo "ERROR: VM process $$PID is not running"; \
		echo "Start a VM with 'make inception-vm' first."; \
		rm -f $(INCEPTION_VM_PID); \
		exit 1; \
	fi; \
	echo "==> Running in VM (PID $$PID): $(CMD)"; \
	sudo ./target/release/fcvm exec --pid $$PID -- $(CMD)

# Wait for VM to be ready and then run a command
# Usage: make inception-wait-exec CMD="/mnt/fcvm-btrfs/check_kvm_caps"
inception-wait-exec: build
	@echo "==> Waiting for inception VM to be ready..."
	@if [ ! -f $(INCEPTION_VM_PID) ]; then \
		echo "ERROR: No PID file found. Start a VM with 'make inception-vm &' first."; \
		exit 1; \
	fi; \
	PID=$$(cat $(INCEPTION_VM_PID)); \
	for i in $$(seq 1 30); do \
		if ! kill -0 $$PID 2>/dev/null; then \
			echo "ERROR: VM process $$PID exited"; \
			rm -f $(INCEPTION_VM_PID); \
			exit 1; \
		fi; \
		if sudo ./target/release/fcvm exec --pid $$PID -- true 2>/dev/null; then \
			echo "==> VM ready (PID $$PID)"; \
			echo "==> Running: $(CMD)"; \
			sudo ./target/release/fcvm exec --pid $$PID -- $(CMD); \
			exit 0; \
		fi; \
		sleep 2; \
		echo "  Waiting... ($$i/30)"; \
	done; \
	echo "ERROR: Timeout waiting for VM to be ready"; \
	exit 1

# Stop the inception VM
inception-stop:
	@if [ -f $(INCEPTION_VM_PID) ]; then \
		PID=$$(cat $(INCEPTION_VM_PID)); \
		if kill -0 $$PID 2>/dev/null; then \
			echo "==> Stopping VM (PID $$PID)..."; \
			sudo kill $$PID 2>/dev/null || true; \
			sleep 1; \
			if kill -0 $$PID 2>/dev/null; then \
				echo "==> Force killing..."; \
				sudo kill -9 $$PID 2>/dev/null || true; \
			fi; \
			echo "==> VM stopped."; \
		else \
			echo "==> VM process $$PID not running (stale PID file)"; \
		fi; \
		rm -f $(INCEPTION_VM_PID); \
	else \
		echo "==> No PID file found. No VM to stop."; \
	fi

# Show VM status
inception-status:
	@echo "=== Inception VM Status ==="
	@if [ -f $(INCEPTION_VM_PID) ]; then \
		PID=$$(cat $(INCEPTION_VM_PID)); \
		if kill -0 $$PID 2>/dev/null; then \
			echo "VM PID: $$PID (running)"; \
			ps -p $$PID -o pid,ppid,user,%cpu,%mem,etime,cmd --no-headers 2>/dev/null || true; \
		else \
			echo "VM PID: $$PID (NOT running - stale PID file)"; \
			rm -f $(INCEPTION_VM_PID); \
		fi; \
	else \
		echo "No PID file found at $(INCEPTION_VM_PID)"; \
		echo "No VM running."; \
	fi
