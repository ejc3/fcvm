#!/bin/bash
# Build btrfs-progs >= 6.12 for --rootdir --subvol support.
#
# btrfs-progs 6.12+ includes mkfs.btrfs --rootdir --subvol which creates
# btrfs images from directories containing btrfs subvolumes — without root.
#
# Builds inside an Ubuntu Noble container to work on any host OS (EL9, Ubuntu, etc.).
# Uses the same proxy pattern as fcvm's rootfs package download.
#
# Usage: ./scripts/build-btrfs-progs.sh [output_dir]
#        Output defaults to /mnt/fcvm-btrfs/deps/bin/

set -euo pipefail

BTRFS_PROGS_VERSION="6.12"
OUTPUT_DIR="${1:-/mnt/fcvm-btrfs/deps/bin}"

echo "=== Building btrfs-progs ${BTRFS_PROGS_VERSION} ==="
echo "Output: $OUTPUT_DIR/mkfs.btrfs"
echo ""

# Check if already built and runnable
if [[ -x "$OUTPUT_DIR/mkfs.btrfs" ]]; then
    EXISTING_VERSION=$("$OUTPUT_DIR/mkfs.btrfs" --version 2>/dev/null | grep -oP '[\d.]+' || echo "0")
    MAJOR=$(echo "$EXISTING_VERSION" | cut -d. -f1)
    MINOR=$(echo "$EXISTING_VERSION" | cut -d. -f2)
    if [[ "$MAJOR" -gt 6 ]] || [[ "$MAJOR" -eq 6 && "$MINOR" -ge 12 ]]; then
        echo "mkfs.btrfs $EXISTING_VERSION already exists at $OUTPUT_DIR/mkfs.btrfs (>= 6.12)"
        echo "Delete it to force rebuild."
        exit 0
    fi
fi

mkdir -p "$OUTPUT_DIR"

# Build proxy args for podman (same pattern as fcvm rootfs download)
PROXY_ARGS=()
for var in http_proxy https_proxy HTTP_PROXY HTTPS_PROXY; do
    if [[ -n "${!var:-}" ]]; then
        PROXY_ARGS+=("-e" "${var}=${!var}")
    fi
done

# Determine curl proxy flag from http_proxy
CURL_PROXY=""
if [[ -n "${http_proxy:-}" ]]; then
    CURL_PROXY="-x $http_proxy"
elif [[ -n "${HTTPS_PROXY:-}" ]]; then
    CURL_PROXY="-x $HTTPS_PROXY"
fi

echo "Building in Ubuntu Noble container..."
podman run --rm --cgroups=disabled --network=host \
    "${PROXY_ARGS[@]}" \
    -v "$OUTPUT_DIR:/output:Z" \
    ubuntu:noble bash -c "
set -e
# Configure apt proxy (same as fcvm rootfs download)
echo 'APT::Sandbox::User \"root\";' > /etc/apt/apt.conf.d/10sandbox
if [ -n \"\${http_proxy:-}\" ]; then
    echo \"Acquire::http::Proxy \\\"\$http_proxy\\\";\" > /etc/apt/apt.conf.d/99proxy
    echo \"Acquire::https::Proxy \\\"\$http_proxy\\\";\" >> /etc/apt/apt.conf.d/99proxy
fi
echo 'Acquire::Retries \"3\";' > /etc/apt/apt.conf.d/80retries

echo '  Installing build deps...'
apt-get update -qq >/dev/null 2>&1
apt-get install -y --no-install-recommends >/dev/null 2>&1 \
    curl ca-certificates build-essential autoconf automake pkg-config \
    uuid-dev libblkid-dev liblzo2-dev libzstd-dev zlib1g-dev libext2fs-dev libudev-dev

echo '  Downloading btrfs-progs ${BTRFS_PROGS_VERSION}...'
cd /tmp
curl ${CURL_PROXY} -sL https://github.com/kdave/btrfs-progs/archive/refs/tags/v${BTRFS_PROGS_VERSION}.tar.gz | tar xz
cd btrfs-progs-${BTRFS_PROGS_VERSION}

echo '  Configuring...'
./autogen.sh >/dev/null 2>&1
./configure --disable-documentation --disable-python >/dev/null 2>&1

echo '  Compiling...'
make -j\$(nproc) >/dev/null 2>&1

# Copy the binary AND its required shared libraries so it runs on any host
echo '  Bundling binary + libraries...'
mkdir -p /output/lib
cp mkfs.btrfs /output/mkfs.btrfs.bin
chmod +x /output/mkfs.btrfs.bin

# Copy required shared libraries
for lib in \$(ldd mkfs.btrfs | awk '/=>/ {print \$3}'); do
    cp \"\$lib\" /output/lib/ 2>/dev/null || true
done
# Copy the dynamic linker (architecture-dependent)
# x86_64: ld-linux-x86-64.so.2, aarch64: ld-linux-aarch64.so.1
cp /lib64/ld-linux-*.so.* /output/lib/ 2>/dev/null || \
    cp /lib/x86_64-linux-gnu/ld-linux-*.so.* /output/lib/ 2>/dev/null || \
    cp /lib/aarch64-linux-gnu/ld-linux-*.so.* /output/lib/ 2>/dev/null || true

echo '  Done!'
./mkfs.btrfs --version
"

# Create wrapper script that uses bundled libraries (architecture-independent)
cat > "$OUTPUT_DIR/mkfs.btrfs" << 'WRAPPER'
#!/bin/bash
DIR="$(cd "$(dirname "$0")" && pwd)"
# Find the dynamic linker (works on both x86_64 and aarch64)
LD_LINUX=$(find "$DIR/lib" -name 'ld-linux-*.so.*' -type f 2>/dev/null | head -1)
if [ -z "$LD_LINUX" ]; then
    echo "Error: ld-linux not found in $DIR/lib" >&2
    exit 1
fi
exec "$LD_LINUX" --library-path "$DIR/lib" "$DIR/mkfs.btrfs.bin" "$@"
WRAPPER
chmod +x "$OUTPUT_DIR/mkfs.btrfs"

echo ""
echo "=== Verifying ==="
"$OUTPUT_DIR/mkfs.btrfs" --version

echo ""
echo "=== Success! ==="
echo "Binary: $OUTPUT_DIR/mkfs.btrfs"
