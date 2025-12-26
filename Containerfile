FROM docker.io/library/rust:1.83-bookworm

# Install Rust toolchain from rust-toolchain.toml
COPY rust-toolchain.toml /tmp/rust-toolchain.toml
RUN RUST_VERSION=$(grep 'channel' /tmp/rust-toolchain.toml | cut -d'"' -f2) && \
    rustup toolchain install $RUST_VERSION && \
    rustup default $RUST_VERSION && \
    rustup component add rustfmt clippy && \
    rustup target add aarch64-unknown-linux-musl x86_64-unknown-linux-musl

# Install cargo tools
RUN cargo install cargo-nextest cargo-audit cargo-deny --locked

# Install system dependencies
RUN apt-get update && apt-get install -y \
    fuse3 libfuse3-dev autoconf automake libtool perl libclang-dev clang \
    musl-tools iproute2 iptables slirp4netns dnsmasq qemu-utils e2fsprogs \
    parted podman skopeo git curl sudo procps zstd busybox-static cpio uidmap \
    && rm -rf /var/lib/apt/lists/*

# Install Firecracker
ARG ARCH=aarch64
RUN curl -fsSL -o /tmp/fc.tgz \
    https://github.com/firecracker-microvm/firecracker/releases/download/v1.14.0/firecracker-v1.14.0-${ARCH}.tgz \
    && tar --no-same-owner -xzf /tmp/fc.tgz -C /tmp \
    && mv /tmp/release-v1.14.0-${ARCH}/firecracker-v1.14.0-${ARCH} /usr/local/bin/firecracker \
    && rm -rf /tmp/fc.tgz /tmp/release-v1.14.0-${ARCH}

# Setup testuser with sudo and namespace support
RUN echo "user_allow_other" >> /etc/fuse.conf \
    && groupadd -f fuse && groupadd -f kvm \
    && useradd -m -s /bin/bash testuser \
    && usermod -aG fuse,kvm testuser \
    && echo "testuser ALL=(ALL) NOPASSWD:ALL" >> /etc/sudoers \
    && echo "testuser:100000:65536" >> /etc/subuid \
    && echo "testuser:100000:65536" >> /etc/subgid

# Symlink cargo tools to /usr/local/bin for sudo
RUN for bin in cargo rustc rustfmt cargo-clippy clippy-driver cargo-nextest cargo-audit cargo-deny; do \
    ln -s /usr/local/cargo/bin/$bin /usr/local/bin/$bin 2>/dev/null || true; done

# Setup workspace
WORKDIR /workspace/fcvm
RUN mkdir -p /workspace/fcvm /workspace/fuse-backend-rs /workspace/fuser \
    && chown -R testuser:testuser /workspace

USER testuser
CMD ["make", "test-unit"]
