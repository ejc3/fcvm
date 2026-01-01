#!/bin/bash
# GitHub Actions Runner Setup Script
# Downloads custom kernel, reboots, then configures runner
set -euxo pipefail

INSTANCE_ID=$(curl -s http://169.254.169.254/latest/meta-data/instance-id)
MARKER=/var/lib/runner-kernel-installed

# Phase 1: Install custom kernel and reboot
if [ ! -f "$MARKER" ]; then
  echo "=== Phase 1: Installing custom host kernel ==="

  apt-get update
  apt-get install -y curl jq

  # Find the latest host kernel release
  RELEASE_TAG=$(curl -s https://api.github.com/repos/ejc3/firepod/releases | \
    jq -r '.[] | select(.tag_name | startswith("host-kernel-")) | .tag_name' | head -1)

  if [ -n "$RELEASE_TAG" ]; then
    echo "Found kernel release: $RELEASE_TAG"

    KERNEL_URL=$(curl -s "https://api.github.com/repos/ejc3/firepod/releases/tags/$RELEASE_TAG" | \
      jq -r '.assets[] | select(.name | startswith("linux-image-")) | .browser_download_url')

    if [ -n "$KERNEL_URL" ]; then
      cd /tmp
      curl -LO "$KERNEL_URL"
      dpkg -i linux-image-*.deb || apt-get install -f -y
      rm -f linux-image-*.deb
      touch "$MARKER"

      # Create systemd service for phase 2
      cat > /etc/systemd/system/runner-setup.service << 'SERVICE'
[Unit]
Description=GitHub Runner Setup (phase 2)
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
ExecStart=/bin/bash -c 'curl -fsSL https://raw.githubusercontent.com/ejc3/firepod/main/scripts/setup-runner.sh | bash'
RemainAfterExit=yes
StandardOutput=journal+console
StandardError=journal+console

[Install]
WantedBy=multi-user.target
SERVICE
      systemctl daemon-reload
      systemctl enable runner-setup.service

      echo "Rebooting to use new kernel..."
      reboot
      exit 0
    fi
  fi

  echo "No custom kernel found, continuing with stock kernel..."
  touch "$MARKER"
fi

# Phase 2: Setup runner (after reboot with new kernel)
echo "=== Phase 2: Setting up GitHub Actions Runner ==="
echo "Kernel: $(uname -r)"

# System packages
apt-get update
apt-get install -y curl wget git jq build-essential \
  podman uidmap slirp4netns fuse-overlayfs containernetworking-plugins \
  fuse3 libfuse3-dev libclang-dev clang musl-tools \
  iproute2 iptables dnsmasq qemu-utils e2fsprogs parted \
  skopeo busybox-static cpio zstd autoconf automake libtool

# Node.js 22.x
curl -fsSL https://deb.nodesource.com/setup_22.x | bash -
apt-get install -y nodejs

# Rust
sudo -u ubuntu bash -c 'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y'
# Symlinks so cargo works under sudo
ln -sf /home/ubuntu/.cargo/bin/cargo /usr/local/bin/cargo
ln -sf /home/ubuntu/.cargo/bin/rustc /usr/local/bin/rustc
ln -sf /home/ubuntu/.cargo/bin/rustup /usr/local/bin/rustup

# Firecracker
FIRECRACKER_VERSION="v1.14.0"
curl -L -o /tmp/firecracker.tgz \
  "https://github.com/firecracker-microvm/firecracker/releases/download/${FIRECRACKER_VERSION}/firecracker-${FIRECRACKER_VERSION}-aarch64.tgz"
tar -xzf /tmp/firecracker.tgz -C /usr/local/bin --strip-components=1 \
  "release-${FIRECRACKER_VERSION}-aarch64/firecracker-${FIRECRACKER_VERSION}-aarch64" \
  "release-${FIRECRACKER_VERSION}-aarch64/jailer-${FIRECRACKER_VERSION}-aarch64"
mv "/usr/local/bin/firecracker-${FIRECRACKER_VERSION}-aarch64" /usr/local/bin/firecracker
mv "/usr/local/bin/jailer-${FIRECRACKER_VERSION}-aarch64" /usr/local/bin/jailer
rm -f /tmp/firecracker.tgz

# Podman rootless
grep -q "ubuntu:100000:65536" /etc/subuid || echo "ubuntu:100000:65536" >> /etc/subuid
grep -q "ubuntu:100000:65536" /etc/subgid || echo "ubuntu:100000:65536" >> /etc/subgid

# KVM and networking setup
chmod 666 /dev/kvm
mkdir -p /var/run/netns
iptables -P FORWARD ACCEPT || true
if [ ! -e /dev/userfaultfd ]; then
  mknod /dev/userfaultfd c 10 126
fi
chmod 666 /dev/userfaultfd
sysctl -w vm.unprivileged_userfaultfd=1
echo "user_allow_other" > /etc/fuse.conf

# GitHub Actions Runner
mkdir -p /opt/actions-runner && cd /opt/actions-runner
RUNNER_VERSION=$(curl -s https://api.github.com/repos/actions/runner/releases/latest | jq -r '.tag_name' | sed 's/v//')
curl -o actions-runner-linux-arm64.tar.gz -L \
  "https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/actions-runner-linux-arm64-${RUNNER_VERSION}.tar.gz"
tar xzf actions-runner-linux-arm64.tar.gz && rm actions-runner-linux-arm64.tar.gz
chown -R ubuntu:ubuntu /opt/actions-runner
./bin/installdependencies.sh

# Register runner
PAT=$(aws ssm get-parameter --name /github-runner/pat --with-decryption --query 'Parameter.Value' --output text --region us-west-1 2>/dev/null || echo "")
if [ -n "$PAT" ] && [ "$PAT" != "placeholder" ]; then
  TOKEN=$(curl -s -X POST -H "Authorization: token $PAT" \
    https://api.github.com/repos/ejc3/firepod/actions/runners/registration-token | jq -r '.token')
  sudo -u ubuntu ./config.sh --url https://github.com/ejc3/firepod --token "$TOKEN" \
    --name "runner-$INSTANCE_ID" --labels self-hosted,Linux,ARM64 --unattended
  ./svc.sh install ubuntu
  ./svc.sh start
  echo "Runner registered and started!"
else
  echo "WARNING: GitHub PAT not configured in SSM. Runner not registered."
fi

# Clean up phase 2 service
systemctl disable runner-setup.service 2>/dev/null || true
rm -f /etc/systemd/system/runner-setup.service

echo "=== Runner setup complete ==="
