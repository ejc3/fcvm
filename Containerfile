# fcvm test container
#
# Build context must include fuse-backend-rs and fuser alongside fcvm:
#   cd ~/fcvm && podman build -t fcvm-test -f Containerfile \
#       --build-context fuse-backend-rs=../fuse-backend-rs \
#       --build-context fuser=../fuser .
#
# Test with: podman run --rm --privileged --device /dev/fuse fcvm-test

FROM docker.io/library/rust:1.83-bookworm

# Copy rust-toolchain.toml to read version from single source of truth
COPY rust-toolchain.toml /tmp/rust-toolchain.toml

# Install toolchain version from rust-toolchain.toml (avoids version drift)
# Edition 2024 is stable since Rust 1.85
# Also add musl targets for statically linked fc-agent (portable across glibc versions)
RUN RUST_VERSION=$(grep 'channel' /tmp/rust-toolchain.toml | cut -d'"' -f2) && \
    rustup toolchain install $RUST_VERSION && \
    rustup default $RUST_VERSION && \
    rustup component add rustfmt clippy && \
    rustup target add aarch64-unknown-linux-musl x86_64-unknown-linux-musl

# Install cargo-nextest for better test parallelism and output
RUN cargo install cargo-nextest --locked

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
    # musl libc for statically linked fc-agent (portable across glibc versions)
    musl-tools \
    # fcvm VM test dependencies
    iproute2 \
    iptables \
    slirp4netns \
    dnsmasq \
    qemu-utils \
    e2fsprogs \
    parted \
    # Utilities
    git \
    curl \
    sudo \
    procps \
    # Required for initrd creation (must be statically linked for kernel boot)
    busybox-static \
    cpio \
    # Clean up
    && rm -rf /var/lib/apt/lists/*

# Download and install Firecracker (architecture-aware)
# v1.14.0 adds network_overrides support for snapshot cloning
ARG ARCH=aarch64
RUN curl -L -o /tmp/firecracker.tgz \
    https://github.com/firecracker-microvm/firecracker/releases/download/v1.14.0/firecracker-v1.14.0-${ARCH}.tgz \
    && tar --no-same-owner -xzf /tmp/firecracker.tgz -C /tmp \
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

# Rust tools are installed system-wide at /usr/local/cargo (owned by root)
# Symlink to /usr/local/bin so sudo can find them (sudo uses secure_path)
RUN ln -s /usr/local/cargo/bin/cargo /usr/local/bin/cargo \
    && ln -s /usr/local/cargo/bin/rustc /usr/local/bin/rustc \
    && ln -s /usr/local/cargo/bin/cargo-nextest /usr/local/bin/cargo-nextest

# Allow testuser to sudo without password (like host dev setup)
RUN echo "testuser ALL=(ALL) NOPASSWD:ALL" >> /etc/sudoers

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

# Switch to testuser - tests run as normal user with sudo like on host
USER testuser

# Default command runs all fuse-pipe tests
CMD ["cargo", "nextest", "run", "--release", "-p", "fuse-pipe"]
