SHELL := /bin/bash

# EC2 build host configuration
EC2_HOST := ubuntu@54.67.60.104
EC2_KEY := ~/.ssh/fcvm-ec2
SSH := ssh -i $(EC2_KEY) $(EC2_HOST)
RSYNC := rsync -avz --delete --exclude 'target' --exclude '.git' -e "ssh -i $(EC2_KEY)"

# Remote paths
REMOTE_DIR := ~/fcvm
REMOTE_KERNEL_DIR := ~/linux-firecracker

# Local output directory for downloaded artifacts
ARTIFACTS := artifacts

.PHONY: all build sync build-remote build-local clean kernel rootfs deploy test test-sanity fuse-pipe-test help

all: build

help:
	@echo "fcvm Build System"
	@echo ""
	@echo "Development (remote EC2 builds):"
	@echo "  make sync          - Sync local code to EC2"
	@echo "  make build         - Sync + build fcvm and fc-agent on EC2"
	@echo "  make build-remote  - Build on EC2 (no sync)"
	@echo "  make fetch         - Download built artifacts from EC2"
	@echo "  make deploy        - Install fc-agent into rootfs"
	@echo ""
	@echo "Kernel (builds on EC2 with FUSE support):"
	@echo "  make kernel-setup  - Clone Linux $(KERNEL_VERSION) and upload config"
	@echo "  make kernel        - Build kernel Image on EC2 (~10-20 min)"
	@echo "  make kernel-config - Show diff from upstream config"
	@echo "  make kernel-fetch  - Download vmlinux from EC2"
	@echo ""
	@echo "Images:"
	@echo "  make rootfs        - Build rootfs image on EC2"
	@echo "  make rebuild       - Sync + build + rebuild rootfs (full rebuild)"
	@echo ""
	@echo "Testing:"
	@echo "  make test          - Run all integration tests on EC2 (cargo test)"
	@echo "  make test-sanity   - Run sanity test (basic VM startup)"
	@echo "  make fuse-pipe-test - Run fuse-pipe library tests"
	@echo ""
	@echo "Local:"
	@echo "  make build-local   - Build locally (macOS, won't run)"
	@echo "  make clean         - Clean local build artifacts"
	@echo ""
	@echo "Build artifacts:"
	@echo "  fcvm:     target/release/fcvm"
	@echo "  fc-agent: target/release/fc-agent"
	@echo "  vmlinux:  /mnt/fcvm-btrfs/kernels/vmlinux (FUSE-enabled)"

#
# Code sync
#
sync:
	@echo "==> Syncing code to EC2..."
	$(RSYNC) . $(EC2_HOST):$(REMOTE_DIR)/

#
# Remote builds (on EC2)
#
build: sync build-remote

build-remote:
	@echo "==> Building workspace on EC2 (fcvm + fc-agent)..."
	$(SSH) "cd $(REMOTE_DIR) && source ~/.cargo/env && cargo build --release 2>&1" | tee /tmp/fcvm-build.log
	@echo "==> Build complete!"
	@echo "    fcvm:     $(REMOTE_DIR)/target/release/fcvm"
	@echo "    fc-agent: $(REMOTE_DIR)/target/release/fc-agent"

#
# Fetch built artifacts
#
fetch:
	@mkdir -p $(ARTIFACTS)
	@echo "==> Downloading fcvm..."
	scp -i $(EC2_KEY) $(EC2_HOST):$(REMOTE_DIR)/target/release/fcvm $(ARTIFACTS)/
	@echo "==> Downloading fc-agent..."
	scp -i $(EC2_KEY) $(EC2_HOST):$(REMOTE_DIR)/target/release/fc-agent $(ARTIFACTS)/
	@echo "==> Artifacts downloaded to $(ARTIFACTS)/"
	@ls -la $(ARTIFACTS)/

#
# Kernel build (with FUSE support)
#
# Using Firecracker's official 5.10 config as base, with FUSE enabled
# Config files in config/:
#   - microvm-kernel-ci-aarch64-5.10.config (original from Firecracker)
#   - microvm-kernel-aarch64-5.10-fuse.config (our version with FUSE)
#
KERNEL_VERSION := 5.10
KERNEL_CONFIG := config/microvm-kernel-aarch64-5.10-fuse.config

kernel-setup:
	@echo "==> Setting up kernel source on EC2..."
	$(SSH) "sudo apt-get update && sudo apt-get install -y build-essential libncurses-dev bison flex libssl-dev libelf-dev bc"
	$(SSH) "[ -d $(REMOTE_KERNEL_DIR) ] || git clone --depth 1 --branch linux-$(KERNEL_VERSION).y https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git $(REMOTE_KERNEL_DIR)"
	@echo "==> Uploading kernel config (with FUSE support)..."
	scp -i $(EC2_KEY) $(KERNEL_CONFIG) $(EC2_HOST):$(REMOTE_KERNEL_DIR)/.config
	@echo "==> Kernel source ready at $(REMOTE_KERNEL_DIR)"
	@echo "==> Config: $(KERNEL_CONFIG)"

kernel:
	@echo "==> Building kernel on EC2 (this takes ~10-20 min)..."
	$(SSH) "cd $(REMOTE_KERNEL_DIR) && make olddefconfig && make -j\$$(nproc) Image 2>&1" | tee /tmp/kernel-build.log
	@echo "==> Kernel built!"
	@echo "==> Installing to /mnt/fcvm-btrfs/kernels/"
	$(SSH) "sudo mkdir -p /mnt/fcvm-btrfs/kernels"
	$(SSH) "sudo cp $(REMOTE_KERNEL_DIR)/arch/arm64/boot/Image /mnt/fcvm-btrfs/kernels/vmlinux-$(KERNEL_VERSION).y-fuse"
	$(SSH) "sudo ln -sf vmlinux-$(KERNEL_VERSION).y-fuse /mnt/fcvm-btrfs/kernels/vmlinux"
	@echo "==> Kernel installed: /mnt/fcvm-btrfs/kernels/vmlinux-$(KERNEL_VERSION).y-fuse"

kernel-config:
	@echo "==> Current kernel config differences from upstream:"
	@diff -u config/microvm-kernel-ci-aarch64-5.10.config config/microvm-kernel-aarch64-5.10-fuse.config || true

kernel-fetch:
	@mkdir -p $(ARTIFACTS)
	@echo "==> Downloading vmlinux..."
	scp -i $(EC2_KEY) $(EC2_HOST):/mnt/fcvm-btrfs/kernels/vmlinux $(ARTIFACTS)/
	@ls -la $(ARTIFACTS)/vmlinux

#
# Rootfs build (updates fc-agent in existing base image)
#
rootfs:
	@echo "==> Updating fc-agent in rootfs on EC2..."
	$(SSH) "sudo mkdir -p /tmp/rootfs-mount && \
		sudo mount -o loop /mnt/fcvm-btrfs/rootfs/base.ext4 /tmp/rootfs-mount && \
		sudo cp $(REMOTE_DIR)/target/release/fc-agent /tmp/rootfs-mount/usr/local/bin/fc-agent && \
		sudo chmod +x /tmp/rootfs-mount/usr/local/bin/fc-agent && \
		sudo umount /tmp/rootfs-mount"
	@echo "==> Rootfs updated: /mnt/fcvm-btrfs/rootfs/base.ext4"

#
# Fresh rootfs build (creates new Debian rootfs via fcvm setup)
#
rootfs-fresh:
	@echo "==> Creating fresh Debian rootfs on EC2..."
	@echo "    This will take ~90 seconds (debootstrap + package installation)"
	$(SSH) "cd $(REMOTE_DIR) && sudo rm -f /mnt/fcvm-btrfs/rootfs/base.ext4 && sudo $(FCVM_BIN) setup rootfs 2>&1" | tee /tmp/rootfs-fresh.log
	@echo "==> Fresh rootfs created: /mnt/fcvm-btrfs/rootfs/base.ext4"

#
# Full rebuild: build binaries + rebuild rootfs
#
rebuild: build rootfs
	@echo "==> Full rebuild complete!"

#
# Deploy (install fc-agent into existing rootfs - for quick updates)
#
deploy:
	@echo "==> Deploying fc-agent to rootfs..."
	$(SSH) "sudo cp $(REMOTE_DIR)/target/release/fc-agent /mnt/fcvm-btrfs/rootfs/base/usr/local/bin/"
	@echo "==> fc-agent deployed"

#
# Testing (all tests are now cargo integration tests)
#

# Run all integration tests
test: build
	@echo "==> Running all integration tests on EC2..."
	$(SSH) "cd $(REMOTE_DIR) && sudo ~/.cargo/bin/cargo test 2>&1" | tee /tmp/test.log

# Run sanity test (basic VM startup)
test-sanity: build
	@echo "==> Running sanity test on EC2..."
	$(SSH) "cd $(REMOTE_DIR) && sudo ~/.cargo/bin/cargo test --test test_sanity 2>&1" | tee /tmp/test-sanity.log

# Run fuse-pipe library tests
fuse-pipe-test: sync
	@echo "==> Running fuse-pipe tests on EC2..."
	$(SSH) "cd $(REMOTE_DIR)/fuse-pipe && source ~/.cargo/env && cargo test 2>&1" | tee /tmp/fuse-pipe-test.log

#
# Local builds (for IDE/linting only - won't run on macOS)
#
build-local:
	cargo build --release

clean:
	cargo clean
	rm -rf $(ARTIFACTS)

#
# Watch for changes and rebuild
#
watch:
	@echo "Watching for changes... (Ctrl+C to stop)"
	@while true; do \
		fswatch -1 src fc-agent/src Cargo.toml fc-agent/Cargo.toml && \
		make build; \
	done
