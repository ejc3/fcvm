#!/bin/sh
# Recursive inception script for nested virtualization testing
# Usage: inception <current_level> <max_level> <kernel_path> <image_cache_path>
set -e

LEVEL=$1
MAX=$2
KERNEL=$3
IMAGE_CACHE=$4

echo "[L${LEVEL}] Starting level ${LEVEL} of ${MAX} (CAS verified)"

# Start nginx for health checks (this script overrides the default CMD)
mkdir -p /run/netns /run/containers/storage
nginx 2>/dev/null || true

if [ "$LEVEL" -ge "$MAX" ]; then
    echo "INCEPTION_CHAIN_${MAX}_LEVELS_SUCCESS"
    exit 0
fi

# Setup for nested VM
modprobe tun 2>/dev/null || true
mkdir -p /dev/net
mknod /dev/net/tun c 10 200 2>/dev/null || true
chmod 666 /dev/net/tun 2>/dev/null || true

# Import image if needed
if [ -d "$IMAGE_CACHE" ] && ! podman image exists localhost/inception-test 2>/dev/null; then
    echo "[L${LEVEL}] Importing inception image from $IMAGE_CACHE..."
    echo "[L${LEVEL}] Checking cache dir contents..."
    ls -la "$IMAGE_CACHE" 2>&1 || echo "[L${LEVEL}] Failed to list cache dir"
    echo "[L${LEVEL}] Running skopeo..."
    if ! skopeo copy "dir:$IMAGE_CACHE" containers-storage:localhost/inception-test 2>&1; then
        echo "[L${LEVEL}] SKOPEO FAILED!"
        exit 1
    fi
    echo "[L${LEVEL}] Import complete"
fi

# Calculate next level
NEXT=$((LEVEL + 1))

# Calculate resources for nested level to prevent OOM.
# FUSE readers: memory per mount = readers × 8MB stack
# VM memory: reduce at each level to fit in parent's memory
case $NEXT in
    1) READERS=64; MEM=2048 ;;
    2) READERS=64; MEM=1536 ;;
    3) READERS=8;  MEM=768 ;;
    *) READERS=4;  MEM=512 ;;
esac

echo "[L${LEVEL}] Starting nested VM (L${NEXT}) with ${READERS} FUSE readers and ${MEM}MB RAM..."

# fcvm now automatically puts sockets in /tmp/fcvm-sockets (local) and
# disks in data_dir (FUSE). No loopback btrfs needed!
# Generic env vars: FCVM_FIRECRACKER_BIN, FCVM_FIRECRACKER_ARGS, FCVM_BOOT_ARGS
FCVM_FIRECRACKER_BIN=/usr/local/bin/firecracker-nv2 \
FCVM_FIRECRACKER_ARGS="--enable-nv2" \
FCVM_BOOT_ARGS="kvm-arm.mode=nested numa=off arm64.nv2" \
FCVM_FUSE_READERS=$READERS \
/usr/local/bin/fcvm podman run \
    --name "inception-L${NEXT}-$$" \
    --network bridged \
    --kernel "$KERNEL" \
    --privileged \
    --mem "$MEM" \
    --map /mnt/fcvm-btrfs:/mnt/fcvm-btrfs \
    --map /root/.config/fcvm:/root/.config/fcvm:ro \
    --cmd "inception $NEXT $MAX $KERNEL $IMAGE_CACHE" \
    localhost/inception-test
