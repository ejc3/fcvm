SHELL := /bin/bash

# EC2 build host configuration
EC2_HOST := ubuntu@54.67.60.104
EC2_KEY := ~/.ssh/fcvm-ec2
SSH := ssh -i $(EC2_KEY) $(EC2_HOST)
RSYNC := rsync -avz --delete --exclude 'target' --exclude '.git' -e "ssh -i $(EC2_KEY)"

# Remote paths
REMOTE_DIR := ~/fcvm
REMOTE_KERNEL_DIR := ~/linux-firecracker
REMOTE_FUSE_BACKEND_RS := /home/ubuntu/fuse-backend-rs
LOCAL_FUSE_BACKEND_RS := ../fuse-backend-rs
REMOTE_FUSER := /home/ubuntu/fuser
LOCAL_FUSER := ../fuser

# Container image name
CONTAINER_IMAGE := fcvm-test

# Test commands (same tests, container runs as root so no sudo needed)
TEST_UNIT := cargo test --release --lib
TEST_FUSE_INTEGRATION := cargo test --release -p fuse-pipe --test integration
TEST_FUSE_STRESS := cargo test --release -p fuse-pipe --test test_mount_stress
TEST_FUSE_PERMISSION := cargo test --release -p fuse-pipe --test test_permission_edge_cases
TEST_PJDFSTEST := cargo test --release -p fuse-pipe --test pjdfstest_full -- --nocapture
TEST_VM := cargo test --release --test test_sanity -- --nocapture

# Benchmark commands
BENCH_THROUGHPUT := cargo bench -p fuse-pipe --bench throughput
BENCH_OPERATIONS := cargo bench -p fuse-pipe --bench operations
BENCH_PROTOCOL := cargo bench -p fuse-pipe --bench protocol

# Native needs sudo for FUSE tests (container already runs as root)
SUDO := sudo

.PHONY: all help sync build clean \
        test test-unit test-fuse test-vm test-pjdfstest test-all \
        bench bench-throughput bench-operations bench-protocol \
        rootfs rebuild \
        container-build container-test container-test-fcvm container-test-pjdfstest \
        container-bench container-bench-throughput container-bench-operations container-bench-protocol \
        container-shell \
        setup-btrfs setup-kernel setup-rootfs setup-all

all: build

help:
	@echo "fcvm Build System"
	@echo ""
	@echo "Development:"
	@echo "  make build       - Sync code + build on EC2"
	@echo "  make sync        - Sync code only (no build)"
	@echo "  make clean       - Clean build artifacts"
	@echo ""
	@echo "Testing:"
	@echo "  make test            - All fast tests: unit + fuse-pipe (~115 tests, ~30s)"
	@echo "  make test-unit       - Unit tests only (no root needed)"
	@echo "  make test-fuse       - fuse-pipe tests (integration + permission + stress)"
	@echo "  make test-vm         - VM tests (sanity bridged + rootless)"
	@echo "  make test-pjdfstest  - POSIX compliance (8789 tests, ~5 min)"
	@echo "  make test-all        - Everything: test + test-vm + test-pjdfstest"
	@echo ""
	@echo "Benchmarks:"
	@echo "  make bench           - All benchmarks (throughput + operations + protocol)"
	@echo "  make bench-throughput - I/O throughput benchmarks"
	@echo "  make bench-operations - FUSE operation latency benchmarks"
	@echo "  make bench-protocol  - Wire protocol benchmarks"
	@echo ""
	@echo "Container (encapsulated environment):"
	@echo "  make container-test          - fuse-pipe tests"
	@echo "  make container-test-fcvm     - VM tests"
	@echo "  make container-test-pjdfstest - POSIX compliance (8789 tests)"
	@echo "  make container-bench         - All benchmarks"
	@echo "  make container-shell         - Interactive shell"
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
	@echo "==> Syncing code to EC2..."
	@$(RSYNC) . $(EC2_HOST):$(REMOTE_DIR)/
	@$(RSYNC) $(LOCAL_FUSE_BACKEND_RS)/ $(EC2_HOST):$(REMOTE_FUSE_BACKEND_RS)/
	@$(RSYNC) $(LOCAL_FUSER)/ $(EC2_HOST):$(REMOTE_FUSER)/

build: sync
	@echo "==> Building on EC2..."
	@$(SSH) "cd $(REMOTE_DIR) && source ~/.cargo/env && cargo build --release"

clean:
	cargo clean

#------------------------------------------------------------------------------
# Testing (native) - uses same commands as container tests
#------------------------------------------------------------------------------

# Fast tests: unit tests + fuse-pipe tests (no VM needed)
test: build
	@echo "==> Running tests..."
	$(SSH) "cd $(REMOTE_DIR) && $(TEST_UNIT)"
	$(SSH) "cd $(REMOTE_DIR) && $(SUDO) $(TEST_FUSE_INTEGRATION)"
	$(SSH) "cd $(REMOTE_DIR) && $(SUDO) $(TEST_FUSE_STRESS)"
	$(SSH) "cd $(REMOTE_DIR) && $(SUDO) $(TEST_FUSE_PERMISSION)"

# Unit tests only
test-unit: build
	$(SSH) "cd $(REMOTE_DIR) && $(TEST_UNIT)"

# All fuse-pipe tests
test-fuse: build
	$(SSH) "cd $(REMOTE_DIR) && $(SUDO) $(TEST_FUSE_INTEGRATION)"
	$(SSH) "cd $(REMOTE_DIR) && $(SUDO) $(TEST_FUSE_STRESS)"
	$(SSH) "cd $(REMOTE_DIR) && $(SUDO) $(TEST_FUSE_PERMISSION)"

# VM tests (require KVM + setup)
test-vm: build setup-kernel
	$(SSH) "cd $(REMOTE_DIR) && $(SUDO) $(TEST_VM)"

# Full POSIX compliance tests (8789 tests)
test-pjdfstest: build
	$(SSH) "cd $(REMOTE_DIR) && $(SUDO) $(TEST_PJDFSTEST)"

# Run everything
test-all: test test-vm test-pjdfstest

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

# Container run options for fuse-pipe tests
CONTAINER_RUN_FUSE := sudo podman run --rm --privileged \
	--device /dev/fuse \
	--cap-add=MKNOD \
	--device-cgroup-rule='b *:* rwm' \
	--device-cgroup-rule='c *:* rwm' \
	--ulimit nofile=65536:65536 \
	--ulimit nproc=65536:65536 \
	--pids-limit=-1

# Container run options for fcvm tests (adds KVM, btrfs, netns)
CONTAINER_RUN_FCVM := sudo podman run --rm --privileged \
	--device /dev/kvm \
	--device /dev/fuse \
	-v /mnt/fcvm-btrfs:/mnt/fcvm-btrfs \
	-v /var/run/netns:/var/run/netns:rshared \
	--network host

container-build: sync
	@echo "==> Building container..."
	$(SSH) "cd $(REMOTE_DIR) && sudo podman build -t $(CONTAINER_IMAGE) -f Containerfile \
		--build-context fuse-backend-rs=$(REMOTE_FUSE_BACKEND_RS) \
		--build-context fuser=$(REMOTE_FUSER) ."

# Container tests - uses same commands as native tests
container-test: container-build
	@echo "==> Running tests..."
	$(SSH) "$(CONTAINER_RUN_FUSE) $(CONTAINER_IMAGE) $(TEST_UNIT)"
	$(SSH) "$(CONTAINER_RUN_FUSE) $(CONTAINER_IMAGE) $(TEST_FUSE_INTEGRATION)"
	$(SSH) "$(CONTAINER_RUN_FUSE) $(CONTAINER_IMAGE) $(TEST_FUSE_STRESS)"
	$(SSH) "$(CONTAINER_RUN_FUSE) $(CONTAINER_IMAGE) $(TEST_FUSE_PERMISSION)"

container-test-fcvm: container-build setup-kernel
	$(SSH) "$(CONTAINER_RUN_FCVM) $(CONTAINER_IMAGE) $(TEST_VM)"

container-test-pjdfstest: container-build
	$(SSH) "$(CONTAINER_RUN_FUSE) $(CONTAINER_IMAGE) $(TEST_PJDFSTEST)"

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
