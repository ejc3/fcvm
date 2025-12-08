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

.PHONY: all help sync build test test-sanity rootfs rebuild clean \
        container-build container-test container-test-fcvm container-shell \
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
	@echo "  make test              - Run all tests (native on EC2)"
	@echo "  make test-sanity       - Run VM sanity test (native on EC2)"
	@echo "  make container-test    - Run fuse-pipe tests (in container)"
	@echo "  make container-test-fcvm - Run fcvm VM tests (in container)"
	@echo "  make container-shell   - Interactive shell in container"
	@echo ""
	@echo "Setup (idempotent, run automatically by tests):"
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
# Native testing (direct on EC2)
#------------------------------------------------------------------------------

test: build setup-kernel
	@echo "==> Running all tests..."
	$(SSH) "cd $(REMOTE_DIR) && sudo ~/.cargo/bin/cargo test --release"

test-sanity: build setup-kernel
	@echo "==> Running sanity test..."
	$(SSH) "cd $(REMOTE_DIR) && sudo ~/.cargo/bin/cargo test --release --test test_sanity -- --nocapture"

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

# Run fuse-pipe tests in container
container-test: container-build
	@echo "==> Running fuse-pipe tests..."
	$(SSH) "$(CONTAINER_RUN_FUSE) $(CONTAINER_IMAGE) cargo test --release -p fuse-pipe"

# Run fcvm VM tests in container
container-test-fcvm: container-build setup-kernel
	@echo "==> Running fcvm tests..."
	$(SSH) "$(CONTAINER_RUN_FCVM) $(CONTAINER_IMAGE) cargo test --release -p fcvm --test test_sanity -- --nocapture"

container-shell: container-build
	@echo "==> Opening shell..."
	$(SSH) -t "$(CONTAINER_RUN_FUSE) -it $(CONTAINER_IMAGE) bash"
