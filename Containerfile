# fcvm test container
#
# Build context must include fuse-backend-rs and fuser alongside fcvm:
#   cd ~/fcvm && podman build -t fcvm-test -f Containerfile \
#       --build-context fuse-backend-rs=../fuse-backend-rs \
#       --build-context fuser=../fuser .
#
# Test with: podman run --rm --privileged --device /dev/fuse fcvm-test

FROM docker.io/library/rust:1.83-bookworm

# Install nightly toolchain for fuser (requires edition2024)
RUN rustup toolchain install nightly && rustup default nightly

# Install system dependencies
RUN apt-get update && apt-get install -y \
    # FUSE support
    fuse3 \
    libfuse3-dev \
    # pjdfstest build deps
    autoconf \
    automake \
    libtool \
    # pjdfstest runtime deps
    perl \
    # Build deps for bindgen (userfaultfd-sys)
    libclang-dev \
    clang \
    # fcvm VM test dependencies
    iproute2 \
    iptables \
    slirp4netns \
    dnsmasq \
    qemu-utils \
    libguestfs-tools \
    e2fsprogs \
    parted \
    # Utilities
    git \
    curl \
    sudo \
    procps \
    # Clean up
    && rm -rf /var/lib/apt/lists/*

# Download and install Firecracker (architecture-aware)
# v1.14.0 adds network_overrides support for snapshot cloning
ARG ARCH=aarch64
RUN curl -L -o /tmp/firecracker.tgz \
    https://github.com/firecracker-microvm/firecracker/releases/download/v1.14.0/firecracker-v1.14.0-${ARCH}.tgz \
    && tar -xzf /tmp/firecracker.tgz -C /tmp \
    && mv /tmp/release-v1.14.0-${ARCH}/firecracker-v1.14.0-${ARCH} /usr/local/bin/firecracker \
    && chmod +x /usr/local/bin/firecracker \
    && rm -rf /tmp/firecracker.tgz /tmp/release-v1.14.0-${ARCH}

# Build and install pjdfstest (tests expect it at /tmp/pjdfstest-check/)
RUN git clone --depth 1 https://github.com/pjd/pjdfstest /tmp/pjdfstest-check \
    && cd /tmp/pjdfstest-check \
    && autoreconf -ifs \
    && ./configure \
    && make

# Create non-root test user with access to fuse group
RUN groupadd -f fuse \
    && useradd -m -s /bin/bash testuser \
    && usermod -aG fuse testuser

# Configure subordinate UIDs/GIDs for rootless user namespaces
# testuser (UID 1000) gets subordinate range 100000-165535 (65536 IDs)
# This enables `unshare --user --map-auto` without root
RUN echo "testuser:100000:65536" >> /etc/subuid \
    && echo "testuser:100000:65536" >> /etc/subgid

# Install uidmap package for newuidmap/newgidmap setuid helpers
# These are required for --map-auto to work
RUN apt-get update && apt-get install -y uidmap && rm -rf /var/lib/apt/lists/*

# Create workspace structure matching local paths
# Source code is mounted at runtime, not copied - ensures code is always fresh
WORKDIR /workspace

# Create directories that will be mount points
RUN mkdir -p /workspace/fcvm /workspace/fuse-backend-rs /workspace/fuser

# Make workspace owned by testuser for non-root tests
RUN chown -R testuser:testuser /workspace

WORKDIR /workspace/fcvm

# No entrypoint needed - non-root tests run with --user testuser,
# root tests run as root. Volumes get correct ownership automatically.

# Default command runs all fuse-pipe tests
CMD ["cargo", "test", "--release", "-p", "fuse-pipe"]
