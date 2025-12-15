SHELL := /bin/bash

# EC2 build host configuration
EC2_HOST := ubuntu@54.67.60.104
EC2_KEY := ~/.ssh/fcvm-ec2

# Detect if running on EC2 (SSH key only exists on local machine)
ifeq ($(wildcard $(EC2_KEY)),)
    # On EC2: run commands locally
    ON_EC2 := 1
    SSH := sh -c
    RSYNC := true
    REMOTE_DIR := ~/fcvm
else
    # On local: SSH to EC2
    ON_EC2 :=
    SSH := ssh -i $(EC2_KEY) $(EC2_HOST)
    RSYNC := rsync -avz --delete --exclude 'target' --exclude '.git' -e "ssh -i $(EC2_KEY)"
    RSYNC_WITH_GIT := rsync -avz --delete --exclude 'target' -e "ssh -i $(EC2_KEY)"
    REMOTE_DIR := ~/fcvm
endif

# Remote paths (same whether local or EC2)
REMOTE_KERNEL_DIR := ~/linux-firecracker
REMOTE_FUSE_BACKEND_RS := /home/ubuntu/fuse-backend-rs
LOCAL_FUSE_BACKEND_RS := ../fuse-backend-rs
REMOTE_FUSER := /home/ubuntu/fuser
LOCAL_FUSER := ../fuser

# Container image name
CONTAINER_IMAGE := fcvm-test

# Test commands - organized by root requirement
# No root required:
TEST_UNIT := cargo test --release --lib
TEST_FUSE_NOROOT := cargo test --release -p fuse-pipe --test integration
TEST_FUSE_STRESS := cargo test --release -p fuse-pipe --test test_mount_stress
TEST_VM_ROOTLESS := cargo test --release --test test_sanity test_sanity_rootless -- --nocapture

# Root required:
TEST_FUSE_ROOT := cargo test --release -p fuse-pipe --test integration_root
TEST_FUSE_PERMISSION := cargo test --release -p fuse-pipe --test test_permission_edge_cases
TEST_PJDFSTEST := cargo test --release -p fuse-pipe --test pjdfstest_full -- --nocapture
TEST_VM_BRIDGED := cargo test --release --test test_sanity test_sanity_bridged -- --nocapture

# Legacy alias
TEST_VM := cargo test --release --test test_sanity -- --nocapture

# Benchmark commands
BENCH_THROUGHPUT := cargo bench -p fuse-pipe --bench throughput
BENCH_OPERATIONS := cargo bench -p fuse-pipe --bench operations
BENCH_PROTOCOL := cargo bench -p fuse-pipe --bench protocol

# Native needs sudo for FUSE tests (container already runs as root)
SUDO := sudo

.PHONY: all help sync sync-git build clean \
        test test-noroot test-root test-unit test-fuse test-vm test-vm-rootless test-vm-bridged test-all \
        bench bench-throughput bench-operations bench-protocol bench-quick bench-logs bench-clean \
        lint clippy fmt fmt-check \
        rootfs rebuild \
        container-test container-test-unit container-test-noroot container-test-root container-test-fuse \
        container-test-vm container-test-vm-rootless container-test-vm-bridged container-test-fcvm \
        container-test-pjdfstest container-test-all container-test-allow-other container-build-allow-other \
        container-bench container-bench-throughput container-bench-operations container-bench-protocol \
        container-shell container-clean \
        setup-btrfs setup-kernel setup-rootfs setup-all

# Note: container-build is NOT in .PHONY because it depends on $(CONTAINER_MARKER) file

all: build

help:
	@echo "fcvm Build System"
	@echo ""
	@echo "Development:"
	@echo "  make build       - Sync code + build on EC2"
	@echo "  make sync        - Sync code only (no build)"
	@echo "  make clean       - Clean build artifacts"
	@echo ""
	@echo "Testing (organized by root requirement):"
	@echo "  make test            - All fuse-pipe tests: noroot + root"
	@echo "  make test-noroot     - Tests without root: unit + integration + stress (no sudo)"
	@echo "  make test-root       - Tests requiring root: integration_root (sudo)"
	@echo "  make test-unit       - Unit tests only (no root)"
	@echo "  make test-fuse       - fuse-pipe: integration + permission + stress"
	@echo "  make test-vm         - VM tests: rootless + bridged"
	@echo "  make test-vm-rootless - VM test with slirp4netns (no root)"
	@echo "  make test-vm-bridged  - VM test with bridged networking (sudo)"
	@echo "  make test-all        - Everything: test + test-vm"
	@echo ""
	@echo "Benchmarks:"
	@echo "  make bench           - All benchmarks (throughput + operations + protocol)"
	@echo "  make bench-throughput - I/O throughput benchmarks"
	@echo "  make bench-operations - FUSE operation latency benchmarks"
	@echo "  make bench-protocol  - Wire protocol benchmarks"
	@echo "  make bench-quick     - Quick benchmarks (faster iteration)"
	@echo "  make bench-logs      - View recent benchmark logs/telemetry"
	@echo "  make bench-clean     - Clean benchmark artifacts"
	@echo ""
	@echo "Linting:"
	@echo "  make lint            - Run clippy + fmt-check"
	@echo "  make clippy          - Run cargo clippy"
	@echo "  make fmt             - Format code"
	@echo "  make fmt-check       - Check formatting"
	@echo ""
	@echo "Container (source mounted, always fresh code):"
	@echo "  make container-test              - fuse-pipe tests (noroot + root)"
	@echo "  make container-test-noroot       - Tests as non-root user"
	@echo "  make container-test-root         - Tests as root"
	@echo "  make container-test-unit         - Unit tests only (non-root)"
	@echo "  make container-test-fuse         - All fuse-pipe tests explicitly"
	@echo "  make container-test-vm           - VM tests (rootless + bridged)"
	@echo "  make container-test-vm-rootless  - VM test with slirp4netns"
	@echo "  make container-test-vm-bridged   - VM test with bridged networking"
	@echo "  make container-test-pjdfstest    - POSIX compliance (8789 tests)"
	@echo "  make container-test-all          - Everything: test + vm + pjdfstest"
	@echo "  make container-test-allow-other  - Test AllowOther with fuse.conf"
	@echo "  make container-bench             - All benchmarks"
	@echo "  make container-shell             - Interactive shell"
	@echo "  make container-clean             - Force container rebuild"
	@echo ""
	@echo "Setup (idempotent):"
	@echo "  make setup-all    - Full EC2 setup (btrfs + kernel + rootfs)"
	@echo "  make setup-btrfs  - Create btrfs loopback filesystem"
	@echo "  make setup-kernel - Copy kernel to btrfs"
	@echo "  make setup-rootfs - Create base rootfs (~90 sec on first run)"
	@echo ""
	@echo "Rootfs Updates:"
	@echo "  make rootfs      - Update fc-agent in existing rootfs"
	@echo "  make rebuild     - Full rebuild (build + update rootfs)"

#------------------------------------------------------------------------------
# Setup targets (idempotent)
#------------------------------------------------------------------------------

# Create btrfs loopback filesystem if not mounted
setup-btrfs:
	@$(SSH) "if ! mountpoint -q /mnt/fcvm-btrfs 2>/dev/null; then \
		echo '==> Creating btrfs loopback...'; \
		if [ ! -f /var/fcvm-btrfs.img ]; then \
			sudo truncate -s 20G /var/fcvm-btrfs.img && \
			sudo mkfs.btrfs /var/fcvm-btrfs.img; \
		fi && \
		sudo mkdir -p /mnt/fcvm-btrfs && \
		sudo mount -o loop /var/fcvm-btrfs.img /mnt/fcvm-btrfs && \
		sudo mkdir -p /mnt/fcvm-btrfs/{kernels,rootfs,state,snapshots,vm-disks,cache} && \
		sudo chown -R ubuntu:ubuntu /mnt/fcvm-btrfs && \
		echo '==> btrfs ready at /mnt/fcvm-btrfs'; \
	fi"

# Copy kernel to btrfs (requires setup-btrfs)
setup-kernel: setup-btrfs
	@$(SSH) "if [ ! -f /mnt/fcvm-btrfs/kernels/vmlinux.bin ]; then \
		echo '==> Copying kernel...'; \
		cp $(REMOTE_KERNEL_DIR)/arch/arm64/boot/Image /mnt/fcvm-btrfs/kernels/vmlinux.bin && \
		echo '==> Kernel ready'; \
	fi"

# Create base rootfs if missing (requires build + setup-kernel)
# Rootfs is auto-created by fcvm binary on first VM start
setup-rootfs: build setup-kernel
	@$(SSH) "if [ ! -f /mnt/fcvm-btrfs/rootfs/base.ext4 ]; then \
		echo '==> Creating rootfs (first run, ~90 sec)...'; \
		cd $(REMOTE_DIR) && sudo ./target/release/fcvm podman run --name setup-tmp nginx:alpine & \
		FCVM_PID=\$$!; \
		sleep 120; \
		sudo kill \$$FCVM_PID 2>/dev/null || true; \
		echo '==> Rootfs created'; \
	else \
		echo '==> Rootfs exists'; \
	fi"

# Full setup
setup-all: setup-btrfs setup-kernel setup-rootfs
	@echo "==> Setup complete"

#------------------------------------------------------------------------------
# Build targets
#------------------------------------------------------------------------------

sync:
ifdef ON_EC2
	@echo "==> On EC2, skipping sync"
else
	@echo "==> Syncing code to EC2..."
	@# Generate unique token and write to sync-test to verify incremental builds
	@TOKEN=$$(date +%s%N); \
	mkdir -p sync-test && \
	echo 'const BUILD_TOKEN: &str = "'$$TOKEN'";' > sync-test/main.rs && \
	echo 'fn main() { println!("{}", BUILD_TOKEN); }' >> sync-test/main.rs
	@$(RSYNC) . $(EC2_HOST):$(REMOTE_DIR)/
	@$(RSYNC) $(LOCAL_FUSE_BACKEND_RS)/ $(EC2_HOST):$(REMOTE_FUSE_BACKEND_RS)/
	@$(RSYNC) $(LOCAL_FUSER)/ $(EC2_HOST):$(REMOTE_FUSER)/
	@# Verify incremental build works by building and running sync-test
	@TOKEN=$$(grep 'const BUILD_TOKEN' sync-test/main.rs | cut -d'"' -f2); \
	$(SSH) "cd $(REMOTE_DIR)/sync-test && ~/.cargo/bin/cargo build --release -q 2>/dev/null" && \
	RESULT=$$($(SSH) "$(REMOTE_DIR)/sync-test/target/release/sync-test") && \
	if [ "$$RESULT" = "$$TOKEN" ]; then \
		echo "==> Sync verified (token: $$TOKEN)"; \
	else \
		echo "ERROR: Sync verification failed! Expected $$TOKEN, got $$RESULT"; \
		exit 1; \
	fi
endif

# Sync including .git directory (for using git on EC2)
sync-git:
ifdef ON_EC2
	@echo "==> On EC2, skipping sync"
else
	@echo "==> Syncing code + .git to EC2..."
	@$(RSYNC_WITH_GIT) . $(EC2_HOST):$(REMOTE_DIR)/
	@$(RSYNC_WITH_GIT) $(LOCAL_FUSE_BACKEND_RS)/ $(EC2_HOST):$(REMOTE_FUSE_BACKEND_RS)/
	@$(RSYNC_WITH_GIT) $(LOCAL_FUSER)/ $(EC2_HOST):$(REMOTE_FUSER)/
	@echo "==> Git sync complete"
endif

build: sync
	@echo "==> Building on EC2..."
	@$(SSH) "cd $(REMOTE_DIR) && . ~/.cargo/env && cargo build --release"

clean:
	cargo clean

#------------------------------------------------------------------------------
# Testing (native) - organized by root requirement
#------------------------------------------------------------------------------

# Tests that don't require root (run first for faster feedback)
test-noroot: build
	@echo "==> Running tests (no root required)..."
	$(SSH) "cd $(REMOTE_DIR) && $(TEST_UNIT)"
	$(SSH) "cd $(REMOTE_DIR) && $(TEST_FUSE_NOROOT)"
	$(SSH) "cd $(REMOTE_DIR) && $(TEST_FUSE_STRESS)"

# Tests that require root (permission edge cases moved to container-only due to pjdfstest version)
test-root: build
	@echo "==> Running tests (root required)..."
	$(SSH) "cd $(REMOTE_DIR) && $(SUDO) $(TEST_FUSE_ROOT)"

# All fuse-pipe tests: noroot first, then root
test: test-noroot test-root

# Unit tests only
test-unit: build
	$(SSH) "cd $(REMOTE_DIR) && $(TEST_UNIT)"

# All fuse-pipe tests (explicit)
test-fuse: build
	$(SSH) "cd $(REMOTE_DIR) && $(TEST_FUSE_NOROOT)"
	$(SSH) "cd $(REMOTE_DIR) && $(TEST_FUSE_STRESS)"
	$(SSH) "cd $(REMOTE_DIR) && $(SUDO) $(TEST_FUSE_ROOT)"
	$(SSH) "cd $(REMOTE_DIR) && $(SUDO) $(TEST_FUSE_PERMISSION)"

# VM tests - rootless (no root on host)
test-vm-rootless: build setup-kernel
	$(SSH) "cd $(REMOTE_DIR) && $(TEST_VM_ROOTLESS)"

# VM tests - bridged (requires root for iptables/netns)
test-vm-bridged: build setup-kernel
	$(SSH) "cd $(REMOTE_DIR) && $(SUDO) $(TEST_VM_BRIDGED)"

# All VM tests: rootless first, then bridged
test-vm: test-vm-rootless test-vm-bridged

# Run everything (use container-test-pjdfstest for POSIX compliance)
test-all: test test-vm

#------------------------------------------------------------------------------
# Benchmarks (native)
#------------------------------------------------------------------------------

bench: build
	@echo "==> Running all benchmarks..."
	$(SSH) "cd $(REMOTE_DIR) && $(SUDO) $(BENCH_THROUGHPUT)"
	$(SSH) "cd $(REMOTE_DIR) && $(SUDO) $(BENCH_OPERATIONS)"
	$(SSH) "cd $(REMOTE_DIR) && $(BENCH_PROTOCOL)"

bench-throughput: build
	$(SSH) "cd $(REMOTE_DIR) && $(SUDO) $(BENCH_THROUGHPUT)"

bench-operations: build
	$(SSH) "cd $(REMOTE_DIR) && $(SUDO) $(BENCH_OPERATIONS)"

bench-protocol: build
	$(SSH) "cd $(REMOTE_DIR) && $(BENCH_PROTOCOL)"

bench-quick: build
	@echo "==> Running quick benchmarks..."
	$(SSH) "cd $(REMOTE_DIR) && $(SUDO) cargo bench -p fuse-pipe --bench throughput -- --quick"
	$(SSH) "cd $(REMOTE_DIR) && $(SUDO) cargo bench -p fuse-pipe --bench operations -- --quick"

bench-logs:
	@echo "==> Recent benchmark logs..."
	$(SSH) "ls -lt /tmp/fuse-bench-*.log 2>/dev/null | head -5 || echo 'No logs found'"
	@echo ""
	@echo "==> Latest telemetry..."
	$(SSH) "cat \$$(ls -t /tmp/fuse-bench-telemetry-*.json 2>/dev/null | head -1) 2>/dev/null | jq . || echo 'No telemetry found'"

bench-clean:
	@echo "==> Cleaning benchmark artifacts..."
	$(SSH) "rm -rf $(REMOTE_DIR)/target/criterion"
	$(SSH) "rm -f /tmp/fuse-bench-*.log /tmp/fuse-bench-telemetry-*.json /tmp/fuse-stress*.sock /tmp/fuse-ops-bench-*.sock"

#------------------------------------------------------------------------------
# Linting
#------------------------------------------------------------------------------

lint: clippy fmt-check

clippy: sync
	@echo "==> Running clippy..."
	$(SSH) "cd $(REMOTE_DIR) && . ~/.cargo/env && cargo clippy --all-targets --all-features -- -D warnings"

fmt: sync
	@echo "==> Formatting code..."
	$(SSH) "cd $(REMOTE_DIR) && . ~/.cargo/env && cargo fmt"

fmt-check: sync
	@echo "==> Checking format..."
	$(SSH) "cd $(REMOTE_DIR) && . ~/.cargo/env && cargo fmt -- --check"

#------------------------------------------------------------------------------
# Rootfs management
#------------------------------------------------------------------------------

# Update fc-agent in existing rootfs (use after changing fc-agent code)
rootfs: build
	@echo "==> Updating fc-agent in rootfs..."
	$(SSH) "sudo mkdir -p /tmp/rootfs-mount && \
		sudo mount -o loop /mnt/fcvm-btrfs/rootfs/base.ext4 /tmp/rootfs-mount && \
		sudo cp $(REMOTE_DIR)/target/release/fc-agent /tmp/rootfs-mount/usr/local/bin/fc-agent && \
		sudo chmod +x /tmp/rootfs-mount/usr/local/bin/fc-agent && \
		sudo umount /tmp/rootfs-mount && \
		sudo rmdir /tmp/rootfs-mount"
	@echo "==> fc-agent updated in rootfs"

# Full rebuild: build + update rootfs
rebuild: rootfs
	@echo "==> Rebuild complete"

#------------------------------------------------------------------------------
# Container testing
#------------------------------------------------------------------------------

# Container image - source is mounted at runtime, not copied
# Rebuilds automatically when Containerfile changes (make dependency tracking)
CONTAINER_IMAGE := fcvm-test

# Marker file for container build state
CONTAINER_MARKER := .container-built

# Container run with source mounts (code always fresh, can't run stale)
# Cargo cache goes to testuser's home so non-root builds work
CONTAINER_RUN_BASE := sudo podman run --rm --privileged \
	-v $(REMOTE_DIR):/workspace/fcvm \
	-v $(REMOTE_FUSE_BACKEND_RS):/workspace/fuse-backend-rs \
	-v $(REMOTE_FUSER):/workspace/fuser \
	-v fcvm-cargo-target:/workspace/fcvm/target \
	-v fcvm-cargo-home:/home/testuser/.cargo \
	-e CARGO_HOME=/home/testuser/.cargo

# Container run options for fuse-pipe tests
CONTAINER_RUN_FUSE := $(CONTAINER_RUN_BASE) \
	--device /dev/fuse \
	--cap-add=MKNOD \
	--device-cgroup-rule='b *:* rwm' \
	--device-cgroup-rule='c *:* rwm' \
	--ulimit nofile=65536:65536 \
	--ulimit nproc=65536:65536 \
	--pids-limit=-1

# Container run options for fcvm tests (adds KVM, btrfs, netns)
# Used for bridged mode tests that require root/iptables
CONTAINER_RUN_FCVM := $(CONTAINER_RUN_BASE) \
	--device /dev/kvm \
	--device /dev/fuse \
	-v /mnt/fcvm-btrfs:/mnt/fcvm-btrfs \
	-v /var/run/netns:/var/run/netns:rshared \
	--network host

# Truly rootless container run - matches unprivileged host user exactly
# Runs podman WITHOUT sudo (rootless podman) - this is the true unprivileged test
# Uses separate storage (--root) to avoid conflicts with root-owned storage
# --network host so slirp4netns can bind to loopback addresses (127.x.y.z)
# --security-opt seccomp=unconfined allows unshare syscall (no extra capabilities granted)
# No --privileged, no CAP_SYS_ADMIN - matches real unprivileged user
CONTAINER_RUN_ROOTLESS := podman --root=/tmp/podman-rootless run --rm \
	--security-opt seccomp=unconfined \
	-v $(REMOTE_DIR):/workspace/fcvm \
	-v $(REMOTE_FUSE_BACKEND_RS):/workspace/fuse-backend-rs \
	-v $(REMOTE_FUSER):/workspace/fuser \
	-v fcvm-cargo-target-rootless:/workspace/fcvm/target \
	-v fcvm-cargo-home-rootless:/home/testuser/.cargo \
	-e CARGO_HOME=/home/testuser/.cargo \
	--device /dev/kvm \
	--device /dev/net/tun \
	-v /mnt/fcvm-btrfs:/mnt/fcvm-btrfs \
	--network host

# Build container only when Containerfile changes (make tracks dependency)
$(CONTAINER_MARKER): Containerfile
	@echo "==> Building container (Containerfile changed)..."
	$(RSYNC) Containerfile $(EC2_HOST):$(REMOTE_DIR)/
	$(SSH) "cd $(REMOTE_DIR) && sudo podman build -t $(CONTAINER_IMAGE) -f Containerfile ."
	@touch $@

container-build: sync $(CONTAINER_MARKER)

# Container tests - organized by root requirement
# Non-root tests run with --user testuser to verify they don't need root
# fcvm unit tests with network ops skip themselves when not root
container-test-unit: container-build
	@echo "==> Running unit tests as non-root user..."
	$(SSH) "$(CONTAINER_RUN_FUSE) --user testuser $(CONTAINER_IMAGE) $(TEST_UNIT)"

container-test-noroot: container-build
	@echo "==> Running tests as non-root user..."
	$(SSH) "$(CONTAINER_RUN_FUSE) --user testuser $(CONTAINER_IMAGE) $(TEST_UNIT)"
	$(SSH) "$(CONTAINER_RUN_FUSE) --user testuser $(CONTAINER_IMAGE) $(TEST_FUSE_NOROOT)"
	$(SSH) "$(CONTAINER_RUN_FUSE) --user testuser $(CONTAINER_IMAGE) $(TEST_FUSE_STRESS)"

# Root tests run as root inside container
container-test-root: container-build
	@echo "==> Running tests as root..."
	$(SSH) "$(CONTAINER_RUN_FUSE) $(CONTAINER_IMAGE) $(TEST_FUSE_ROOT)"
	$(SSH) "$(CONTAINER_RUN_FUSE) $(CONTAINER_IMAGE) $(TEST_FUSE_PERMISSION)"

# All fuse-pipe tests (explicit) - matches native test-fuse
container-test-fuse: container-build
	@echo "==> Running all fuse-pipe tests..."
	$(SSH) "$(CONTAINER_RUN_FUSE) --user testuser $(CONTAINER_IMAGE) $(TEST_FUSE_NOROOT)"
	$(SSH) "$(CONTAINER_RUN_FUSE) --user testuser $(CONTAINER_IMAGE) $(TEST_FUSE_STRESS)"
	$(SSH) "$(CONTAINER_RUN_FUSE) $(CONTAINER_IMAGE) $(TEST_FUSE_ROOT)"
	$(SSH) "$(CONTAINER_RUN_FUSE) $(CONTAINER_IMAGE) $(TEST_FUSE_PERMISSION)"

# Test AllowOther with user_allow_other configured (non-root with config)
# Uses separate image with user_allow_other pre-configured
CONTAINER_IMAGE_ALLOW_OTHER := fcvm-test-allow-other

container-build-allow-other: container-build
	@echo "==> Building allow-other container..."
	$(SSH) "cd $(REMOTE_DIR) && sudo podman build -t $(CONTAINER_IMAGE_ALLOW_OTHER) -f Containerfile.allow-other ."

container-test-allow-other: container-build-allow-other
	@echo "==> Testing AllowOther with user_allow_other in fuse.conf..."
	$(SSH) "$(CONTAINER_RUN_FUSE) --user testuser $(CONTAINER_IMAGE_ALLOW_OTHER) cargo test --release -p fuse-pipe --test test_allow_other -- --nocapture"

# All fuse-pipe tests: noroot first, then root
container-test: container-test-noroot container-test-root

# VM tests - rootless (truly unprivileged - no --privileged, runs as testuser)
# Uses CONTAINER_RUN_ROOTLESS which drops privileges to match a normal host user
container-test-vm-rootless: container-build setup-kernel
	$(SSH) "$(CONTAINER_RUN_ROOTLESS) $(CONTAINER_IMAGE) $(TEST_VM_ROOTLESS)"

# VM tests - bridged (requires root for iptables/netns)
container-test-vm-bridged: container-build setup-kernel
	$(SSH) "$(CONTAINER_RUN_FCVM) $(CONTAINER_IMAGE) $(TEST_VM_BRIDGED)"

# All VM tests: rootless first, then bridged
container-test-vm: container-test-vm-rootless container-test-vm-bridged

# Legacy alias (runs both VM tests)
container-test-fcvm: container-test-vm

container-test-pjdfstest: container-build
	$(SSH) "$(CONTAINER_RUN_FUSE) $(CONTAINER_IMAGE) $(TEST_PJDFSTEST)"

# Run everything in container
container-test-all: container-test container-test-vm container-test-pjdfstest

# Container benchmarks - uses same commands as native benchmarks
container-bench: container-build
	@echo "==> Running all benchmarks..."
	$(SSH) "$(CONTAINER_RUN_FUSE) $(CONTAINER_IMAGE) $(BENCH_THROUGHPUT)"
	$(SSH) "$(CONTAINER_RUN_FUSE) $(CONTAINER_IMAGE) $(BENCH_OPERATIONS)"
	$(SSH) "$(CONTAINER_RUN_FUSE) $(CONTAINER_IMAGE) $(BENCH_PROTOCOL)"

container-bench-throughput: container-build
	$(SSH) "$(CONTAINER_RUN_FUSE) $(CONTAINER_IMAGE) $(BENCH_THROUGHPUT)"

container-bench-operations: container-build
	$(SSH) "$(CONTAINER_RUN_FUSE) $(CONTAINER_IMAGE) $(BENCH_OPERATIONS)"

container-bench-protocol: container-build
	$(SSH) "$(CONTAINER_RUN_FUSE) $(CONTAINER_IMAGE) $(BENCH_PROTOCOL)"

container-shell: container-build
	$(SSH) -t "$(CONTAINER_RUN_FUSE) -it $(CONTAINER_IMAGE) bash"

# Force container rebuild (removes marker file)
container-clean:
	rm -f $(CONTAINER_MARKER)
	$(SSH) "sudo podman rmi $(CONTAINER_IMAGE) 2>/dev/null || true"
	$(SSH) "sudo podman volume rm fcvm-cargo-target fcvm-cargo-home 2>/dev/null || true"
