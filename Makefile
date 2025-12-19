SHELL := /bin/bash

# Paths (can be overridden via environment for CI)
FUSE_BACKEND_RS ?= /home/ubuntu/fuse-backend-rs
FUSER ?= /home/ubuntu/fuser
KERNEL_DIR ?= ~/linux-firecracker

# Container image name and architecture
CONTAINER_IMAGE := fcvm-test
CONTAINER_ARCH ?= aarch64

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

# Benchmark commands (fuse-pipe)
BENCH_THROUGHPUT := cargo bench -p fuse-pipe --bench throughput
BENCH_OPERATIONS := cargo bench -p fuse-pipe --bench operations
BENCH_PROTOCOL := cargo bench -p fuse-pipe --bench protocol

# Benchmark commands (fcvm - requires VMs)
BENCH_EXEC := cargo bench --bench exec

.PHONY: all help build clean \
        test test-noroot test-root test-unit test-fuse test-vm test-vm-rootless test-vm-bridged test-all \
        bench bench-throughput bench-operations bench-protocol bench-exec bench-quick bench-logs bench-clean \
        lint clippy fmt fmt-check \
        rootfs rebuild \
        container-test container-test-unit container-test-noroot container-test-root container-test-fuse \
        container-test-vm container-test-vm-rootless container-test-vm-bridged container-test-fcvm \
        container-test-pjdfstest container-test-all container-test-allow-other container-build-allow-other \
        container-bench container-bench-throughput container-bench-operations container-bench-protocol container-bench-exec \
        container-shell container-clean \
        setup-btrfs setup-kernel setup-rootfs setup-all

all: build

help:
	@echo "fcvm Build System"
	@echo ""
	@echo "Development:"
	@echo "  make build       - Build fcvm and fc-agent"
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
	@echo "  make bench           - All fuse-pipe benchmarks"
	@echo "  make bench-throughput - FUSE I/O throughput benchmarks"
	@echo "  make bench-operations - FUSE operation latency benchmarks"
	@echo "  make bench-protocol  - Wire protocol benchmarks"
	@echo "  make bench-exec      - fcvm exec latency (bridged vs rootless)"
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
	@echo "  make container-bench             - All fuse-pipe benchmarks"
	@echo "  make container-bench-exec        - fcvm exec latency (bridged vs rootless)"
	@echo "  make container-shell             - Interactive shell"
	@echo "  make container-clean             - Force container rebuild"
	@echo ""
	@echo "Setup (idempotent):"
	@echo "  make setup-all    - Full setup (btrfs + kernel + rootfs)"
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
	@if ! mountpoint -q /mnt/fcvm-btrfs 2>/dev/null; then \
		echo '==> Creating btrfs loopback...'; \
		if [ ! -f /var/fcvm-btrfs.img ]; then \
			sudo truncate -s 20G /var/fcvm-btrfs.img && \
			sudo mkfs.btrfs /var/fcvm-btrfs.img; \
		fi && \
		sudo mkdir -p /mnt/fcvm-btrfs && \
		sudo mount -o loop /var/fcvm-btrfs.img /mnt/fcvm-btrfs && \
		sudo mkdir -p /mnt/fcvm-btrfs/{kernels,rootfs,state,snapshots,vm-disks,cache} && \
		sudo chown -R $$(id -un):$$(id -gn) /mnt/fcvm-btrfs && \
		echo '==> btrfs ready at /mnt/fcvm-btrfs'; \
	fi

# Copy kernel to btrfs (requires setup-btrfs)
# For local dev: copies from KERNEL_DIR
# For CI (x86_64): downloads pre-built kernel from Firecracker releases
KERNEL_VERSION ?= 5.10.225
setup-kernel: setup-btrfs
	@if [ ! -f /mnt/fcvm-btrfs/kernels/vmlinux.bin ]; then \
		ARCH=$$(uname -m); \
		if [ "$$ARCH" = "x86_64" ] && [ ! -d "$(KERNEL_DIR)" ]; then \
			echo "==> Downloading x86_64 kernel for CI..."; \
			curl -sL "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.11/x86_64/vmlinux-$(KERNEL_VERSION)" \
				-o /mnt/fcvm-btrfs/kernels/vmlinux.bin && \
			echo "==> Kernel ready (downloaded)"; \
		else \
			echo '==> Copying kernel...'; \
			if [ "$$ARCH" = "aarch64" ]; then \
				cp $(KERNEL_DIR)/arch/arm64/boot/Image /mnt/fcvm-btrfs/kernels/vmlinux.bin; \
			else \
				cp $(KERNEL_DIR)/arch/x86/boot/bzImage /mnt/fcvm-btrfs/kernels/vmlinux.bin; \
			fi && \
			echo '==> Kernel ready'; \
		fi \
	fi

# Create base rootfs if missing (requires build + setup-kernel)
# Rootfs is auto-created by fcvm binary on first VM start
setup-rootfs: build setup-kernel
	@if [ ! -f /mnt/fcvm-btrfs/rootfs/base.ext4 ]; then \
		echo '==> Creating rootfs (first run, ~90 sec)...'; \
		sudo ./target/release/fcvm podman run --name setup-tmp nginx:alpine & \
		FCVM_PID=$$!; \
		sleep 120; \
		sudo kill $$FCVM_PID 2>/dev/null || true; \
		echo '==> Rootfs created'; \
	else \
		echo '==> Rootfs exists'; \
	fi

# Full setup
setup-all: setup-btrfs setup-kernel setup-rootfs
	@echo "==> Setup complete"

#------------------------------------------------------------------------------
# Build targets
#------------------------------------------------------------------------------

build:
	@echo "==> Building..."
	cargo build --release

clean:
	cargo clean

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
test-root: build
	@echo "==> Running tests (root required)..."
	sudo $(TEST_FUSE_ROOT)

# All fuse-pipe tests: noroot first, then root
test: test-noroot test-root

# Unit tests only
test-unit: build
	$(TEST_UNIT)

# All fuse-pipe tests (explicit)
test-fuse: build
	$(TEST_FUSE_NOROOT)
	$(TEST_FUSE_STRESS)
	sudo $(TEST_FUSE_ROOT)
	sudo $(TEST_FUSE_PERMISSION)

# VM tests - rootless (no root on host)
test-vm-rootless: build setup-kernel
	$(TEST_VM_ROOTLESS)

# VM tests - bridged (requires root for iptables/netns)
test-vm-bridged: build setup-kernel
	sudo $(TEST_VM_BRIDGED)

# All VM tests: rootless first, then bridged
test-vm: test-vm-rootless test-vm-bridged

# Run everything (use container-test-pjdfstest for POSIX compliance)
test-all: test test-vm

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

bench-exec: build setup-kernel
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
# Rootfs management
#------------------------------------------------------------------------------

# Update fc-agent in existing rootfs (use after changing fc-agent code)
rootfs: build
	@echo "==> Updating fc-agent in rootfs..."
	@sudo mkdir -p /tmp/rootfs-mount && \
		sudo mount -o loop /mnt/fcvm-btrfs/rootfs/base.ext4 /tmp/rootfs-mount && \
		sudo cp ./target/release/fc-agent /tmp/rootfs-mount/usr/local/bin/fc-agent && \
		sudo chmod +x /tmp/rootfs-mount/usr/local/bin/fc-agent && \
		sudo umount /tmp/rootfs-mount && \
		sudo rmdir /tmp/rootfs-mount
	@echo "==> fc-agent updated in rootfs"

# Full rebuild: build + update rootfs
rebuild: rootfs
	@echo "==> Rebuild complete"

#------------------------------------------------------------------------------
# Container testing
#------------------------------------------------------------------------------

# Marker file for container build state
CONTAINER_MARKER := .container-built

# Container run with source mounts (code always fresh, can't run stale)
# Cargo cache goes to testuser's home so non-root builds work
CONTAINER_RUN_BASE := sudo podman run --rm --privileged \
	-v .:/workspace/fcvm \
	-v $(FUSE_BACKEND_RS):/workspace/fuse-backend-rs \
	-v $(FUSER):/workspace/fuser \
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
	-v .:/workspace/fcvm \
	-v $(FUSE_BACKEND_RS):/workspace/fuse-backend-rs \
	-v $(FUSER):/workspace/fuser \
	-v fcvm-cargo-target-rootless:/workspace/fcvm/target \
	-v fcvm-cargo-home-rootless:/home/testuser/.cargo \
	-e CARGO_HOME=/home/testuser/.cargo \
	--device /dev/kvm \
	--device /dev/net/tun \
	-v /mnt/fcvm-btrfs:/mnt/fcvm-btrfs \
	--network host

# Build container only when Containerfile changes (make tracks dependency)
# CONTAINER_ARCH can be overridden: export CONTAINER_ARCH=x86_64 for CI
$(CONTAINER_MARKER): Containerfile
	@echo "==> Building container (Containerfile changed, ARCH=$(CONTAINER_ARCH))..."
	sudo podman build -t $(CONTAINER_IMAGE) -f Containerfile --build-arg ARCH=$(CONTAINER_ARCH) .
	@touch $@

container-build: $(CONTAINER_MARKER)

# Export container image for rootless podman (needed for container-test-vm-rootless)
# Rootless podman has separate image storage, so we export from root and import
CONTAINER_ROOTLESS_MARKER := .container-rootless-imported
$(CONTAINER_ROOTLESS_MARKER): $(CONTAINER_MARKER)
	@echo "==> Exporting container for rootless podman..."
	sudo podman save $(CONTAINER_IMAGE) | podman --root=/tmp/podman-rootless load
	@touch $@

container-build-rootless: $(CONTAINER_ROOTLESS_MARKER)

# Container tests - organized by root requirement
# Non-root tests run with --user testuser to verify they don't need root
# fcvm unit tests with network ops skip themselves when not root
container-test-unit: container-build
	@echo "==> Running unit tests as non-root user..."
	$(CONTAINER_RUN_FUSE) --user testuser $(CONTAINER_IMAGE) $(TEST_UNIT)

container-test-noroot: container-build
	@echo "==> Running tests as non-root user..."
	$(CONTAINER_RUN_FUSE) --user testuser $(CONTAINER_IMAGE) $(TEST_UNIT)
	$(CONTAINER_RUN_FUSE) --user testuser $(CONTAINER_IMAGE) $(TEST_FUSE_NOROOT)
	$(CONTAINER_RUN_FUSE) --user testuser $(CONTAINER_IMAGE) $(TEST_FUSE_STRESS)

# Root tests run as root inside container
container-test-root: container-build
	@echo "==> Running tests as root..."
	$(CONTAINER_RUN_FUSE) $(CONTAINER_IMAGE) $(TEST_FUSE_ROOT)
	$(CONTAINER_RUN_FUSE) $(CONTAINER_IMAGE) $(TEST_FUSE_PERMISSION)

# All fuse-pipe tests (explicit) - matches native test-fuse
container-test-fuse: container-build
	@echo "==> Running all fuse-pipe tests..."
	$(CONTAINER_RUN_FUSE) --user testuser $(CONTAINER_IMAGE) $(TEST_FUSE_NOROOT)
	$(CONTAINER_RUN_FUSE) --user testuser $(CONTAINER_IMAGE) $(TEST_FUSE_STRESS)
	$(CONTAINER_RUN_FUSE) $(CONTAINER_IMAGE) $(TEST_FUSE_ROOT)
	$(CONTAINER_RUN_FUSE) $(CONTAINER_IMAGE) $(TEST_FUSE_PERMISSION)

# Test AllowOther with user_allow_other configured (non-root with config)
# Uses separate image with user_allow_other pre-configured
CONTAINER_IMAGE_ALLOW_OTHER := fcvm-test-allow-other

container-build-allow-other: container-build
	@echo "==> Building allow-other container..."
	sudo podman build -t $(CONTAINER_IMAGE_ALLOW_OTHER) -f Containerfile.allow-other .

container-test-allow-other: container-build-allow-other
	@echo "==> Testing AllowOther with user_allow_other in fuse.conf..."
	$(CONTAINER_RUN_FUSE) --user testuser $(CONTAINER_IMAGE_ALLOW_OTHER) cargo test --release -p fuse-pipe --test test_allow_other -- --nocapture

# All fuse-pipe tests: noroot first, then root
container-test: container-test-noroot container-test-root

# VM tests - rootless (truly unprivileged - no --privileged, runs as testuser)
# Uses CONTAINER_RUN_ROOTLESS which drops privileges to match a normal host user
# Depends on container-build-rootless to export image to rootless podman storage
container-test-vm-rootless: container-build-rootless setup-kernel
	$(CONTAINER_RUN_ROOTLESS) $(CONTAINER_IMAGE) $(TEST_VM_ROOTLESS)

# VM tests - bridged (requires root for iptables/netns)
container-test-vm-bridged: container-build setup-kernel
	$(CONTAINER_RUN_FCVM) $(CONTAINER_IMAGE) $(TEST_VM_BRIDGED)

# All VM tests: rootless first, then bridged
container-test-vm: container-test-vm-rootless container-test-vm-bridged

# Legacy alias (runs both VM tests)
container-test-fcvm: container-test-vm

container-test-pjdfstest: container-build
	$(CONTAINER_RUN_FUSE) $(CONTAINER_IMAGE) $(TEST_PJDFSTEST)

# Run everything in container
container-test-all: container-test container-test-vm container-test-pjdfstest

# Container benchmarks - uses same commands as native benchmarks
container-bench: container-build
	@echo "==> Running all fuse-pipe benchmarks..."
	$(CONTAINER_RUN_FUSE) $(CONTAINER_IMAGE) $(BENCH_THROUGHPUT)
	$(CONTAINER_RUN_FUSE) $(CONTAINER_IMAGE) $(BENCH_OPERATIONS)
	$(CONTAINER_RUN_FUSE) $(CONTAINER_IMAGE) $(BENCH_PROTOCOL)

container-bench-throughput: container-build
	$(CONTAINER_RUN_FUSE) $(CONTAINER_IMAGE) $(BENCH_THROUGHPUT)

container-bench-operations: container-build
	$(CONTAINER_RUN_FUSE) $(CONTAINER_IMAGE) $(BENCH_OPERATIONS)

container-bench-protocol: container-build
	$(CONTAINER_RUN_FUSE) $(CONTAINER_IMAGE) $(BENCH_PROTOCOL)

# fcvm exec benchmarks - requires VMs (uses CONTAINER_RUN_FCVM)
container-bench-exec: container-build setup-kernel
	@echo "==> Running exec benchmarks (bridged vs rootless)..."
	$(CONTAINER_RUN_FCVM) $(CONTAINER_IMAGE) $(BENCH_EXEC)

container-shell: container-build
	$(CONTAINER_RUN_FUSE) -it $(CONTAINER_IMAGE) bash

# Force container rebuild (removes marker file)
container-clean:
	rm -f $(CONTAINER_MARKER) $(CONTAINER_ROOTLESS_MARKER)
	sudo podman rmi $(CONTAINER_IMAGE) 2>/dev/null || true
	sudo podman volume rm fcvm-cargo-target fcvm-cargo-home 2>/dev/null || true
	podman --root=/tmp/podman-rootless rmi $(CONTAINER_IMAGE) 2>/dev/null || true
