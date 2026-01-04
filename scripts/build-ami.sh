#!/bin/bash
# Build GitHub runner AMI with custom kernel
# Called from CI workflow - requires AWS credentials
set -euo pipefail

REGION="${AWS_REGION:-us-west-1}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
KERNEL_DIR="$(dirname "$SCRIPT_DIR")/kernel"

# Compute build hash (from kernel config + patches, matches fcvm's kernel SHA)
compute_hash() {
  cat "$KERNEL_DIR/nested.conf" "$KERNEL_DIR/patches/"*.patch 2>/dev/null | sha256sum | cut -c1-12
}

# Check for existing AMI with matching hash
check_existing_ami() {
  local hash="$1"
  aws ec2 describe-images \
    --region "$REGION" \
    --owners self \
    --filters "Name=tag:BuildHash,Values=$hash" \
    --query 'Images[0].ImageId' --output text
}

# Get latest Ubuntu 24.04 ARM64 AMI
get_base_ami() {
  aws ec2 describe-images \
    --region "$REGION" \
    --owners 099720109477 \
    --filters "Name=name,Values=ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-arm64-server-*" \
    --query 'sort_by(Images, &CreationDate)[-1].ImageId' \
    --output text
}

# Create user data script for AMI builder
create_user_data() {
  cat << 'USERDATA'
#!/bin/bash
exec > >(tee /var/log/ami-build.log) 2>&1
set -euxo pipefail

# Get IMDSv2 token and instance ID
TOKEN=$(curl -X PUT "http://169.254.169.254/latest/api/token" -H "X-aws-ec2-metadata-token-ttl-seconds: 21600" -s)
INSTANCE_ID=$(curl -H "X-aws-ec2-metadata-token: $TOKEN" -s http://169.254.169.254/latest/meta-data/instance-id)

# Install AWS CLI first (needed for tagging)
apt-get update
apt-get install -y unzip curl
curl "https://awscli.amazonaws.com/awscli-exe-linux-aarch64.zip" -o "/tmp/awscliv2.zip"
unzip -q /tmp/awscliv2.zip -d /tmp
/tmp/aws/install

# Error handler - tag instance as failed on any error
tag_failed() {
  echo "BUILD FAILED at line $1"
  aws ec2 create-tags --resources $INSTANCE_ID --tags Key=BuildStatus,Value=failed --region us-west-1 || true
  exit 1
}
trap 'tag_failed $LINENO' ERR

aws ec2 create-tags --resources $INSTANCE_ID --tags Key=BuildStatus,Value=building --region us-west-1

# Install build deps (apt-get update already done above)
apt-get install -y build-essential bc bison flex libssl-dev \
  libelf-dev libncurses-dev libdw-dev debhelper-compat rsync kmod cpio curl jq wget git \
  dwarves \
  podman uidmap slirp4netns fuse-overlayfs containernetworking-plugins \
  fuse3 libfuse3-dev libclang-dev clang musl-tools \
  iproute2 iptables dnsmasq qemu-utils e2fsprogs parted \
  skopeo busybox-static cpio zstd autoconf automake libtool \
  nfs-kernel-server libseccomp-dev

# Clone firepod repo to get kernel config and patches
git clone --depth 1 https://github.com/ejc3/firepod.git /tmp/firepod

# Kernel version
KERNEL_VERSION="6.18.3"
echo "Building kernel version: $KERNEL_VERSION"
aws ec2 create-tags --resources $INSTANCE_ID --tags Key=KernelVersion,Value=$KERNEL_VERSION --region us-west-1

# Build host kernel
cd /tmp
BUILD_SHA=$(cat /tmp/firepod/kernel/nested.conf /tmp/firepod/kernel/patches/*.patch 2>/dev/null | sha256sum | cut -c1-12)
LOCALVERSION="-fcvm-${BUILD_SHA}"

# Download and extract kernel source
curl -fsSL "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-${KERNEL_VERSION}.tar.xz" | tar -xJ
cd "linux-${KERNEL_VERSION}"

# Use running kernel config as base, merge with nested.conf
cp /boot/config-$(uname -r) .config 2>/dev/null || zcat /proc/config.gz > .config 2>/dev/null || true
scripts/kconfig/merge_config.sh -m .config /tmp/firepod/kernel/nested.conf
echo "CONFIG_LOCALVERSION=\"${LOCALVERSION}\"" >> .config
echo "CONFIG_LOCALVERSION_AUTO=n" >> .config
make olddefconfig

# Apply all patches from repo
for patch in /tmp/firepod/kernel/patches/*.patch; do
  [ -f "$patch" ] && patch -p1 < "$patch"
done

# Build deb package
make -j$(nproc) bindeb-pkg LOCALVERSION=""
dpkg -i ../linux-image-*.deb

# Configure GRUB with kvm-arm.mode=nested for NV2 support
if ! grep -q "kvm-arm.mode=nested" /etc/default/grub; then
  sed -i 's/GRUB_CMDLINE_LINUX_DEFAULT="\(.*\)"/GRUB_CMDLINE_LINUX_DEFAULT="\1 kvm-arm.mode=nested"/' /etc/default/grub
  echo "Added kvm-arm.mode=nested to GRUB"
fi

# Set new kernel as default
GRUB_ENTRY="Advanced options for Ubuntu>Ubuntu, with Linux ${KERNEL_VERSION}${LOCALVERSION}"
sed -i "s|^GRUB_DEFAULT=.*|GRUB_DEFAULT=\"${GRUB_ENTRY}\"|" /etc/default/grub
update-grub

# Node.js 22.x
curl -fsSL https://deb.nodesource.com/setup_22.x | bash -
apt-get install -y nodejs

# Rust for ubuntu user
sudo -u ubuntu bash -c 'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y'

# Firecracker
FIRECRACKER_VERSION="v1.14.0"
curl -L -o /tmp/firecracker.tgz \
  "https://github.com/firecracker-microvm/firecracker/releases/download/${FIRECRACKER_VERSION}/firecracker-${FIRECRACKER_VERSION}-aarch64.tgz"
tar -xzf /tmp/firecracker.tgz -C /usr/local/bin --strip-components=1 \
  "release-${FIRECRACKER_VERSION}-aarch64/firecracker-${FIRECRACKER_VERSION}-aarch64" \
  "release-${FIRECRACKER_VERSION}-aarch64/jailer-${FIRECRACKER_VERSION}-aarch64"
mv "/usr/local/bin/firecracker-${FIRECRACKER_VERSION}-aarch64" /usr/local/bin/firecracker
mv "/usr/local/bin/jailer-${FIRECRACKER_VERSION}-aarch64" /usr/local/bin/jailer

# Podman rootless
echo "ubuntu:100000:65536" >> /etc/subuid
echo "ubuntu:100000:65536" >> /etc/subgid

# FUSE config
echo "user_allow_other" > /etc/fuse.conf

# GitHub Actions Runner
mkdir -p /opt/actions-runner
RUNNER_VERSION=$(curl -s https://api.github.com/repos/actions/runner/releases/latest | jq -r '.tag_name' | sed 's/v//')
curl -o /tmp/actions-runner.tar.gz -L \
  "https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/actions-runner-linux-arm64-${RUNNER_VERSION}.tar.gz"
tar xzf /tmp/actions-runner.tar.gz -C /opt/actions-runner
chown -R ubuntu:ubuntu /opt/actions-runner
/opt/actions-runner/bin/installdependencies.sh

# Clean up
apt-get clean
rm -rf /var/lib/apt/lists/* /tmp/* /var/tmp/*

# Signal done
aws ec2 create-tags --resources $INSTANCE_ID --tags Key=BuildStatus,Value=complete --region us-west-1
USERDATA
}

# Wait for instance build to complete
wait_for_build() {
  local instance_id="$1"
  local timeout="${2:-120}"  # 120 iterations * 30s = 60 minutes max

  echo "Waiting for build to complete..."
  for i in $(seq 1 "$timeout"); do
    status=$(aws ec2 describe-tags \
      --region "$REGION" \
      --filters "Name=resource-id,Values=$instance_id" "Name=key,Values=BuildStatus" \
      --query 'Tags[0].Value' --output text)

    # Check for failure immediately
    if [ "$status" = "failed" ]; then
      echo "Build failed! Fetching last 50 lines of log..."
      # Try to get error context via SSM
      cmd_id=$(aws ssm send-command \
        --region "$REGION" \
        --instance-ids "$instance_id" \
        --document-name "AWS-RunShellScript" \
        --parameters 'commands=["tail -50 /var/log/ami-build.log 2>/dev/null || echo No log available"]' \
        --query 'Command.CommandId' --output text 2>/dev/null) || true
      if [ -n "$cmd_id" ]; then
        sleep 3
        aws ssm get-command-invocation \
          --region "$REGION" \
          --command-id "$cmd_id" \
          --instance-id "$instance_id" \
          --query 'StandardOutputContent' --output text 2>/dev/null || true
      fi
      return 1
    fi

    # Get last 3 log lines via SSM for progress visibility
    log_lines=""
    if [ "$status" = "building" ]; then
      cmd_id=$(aws ssm send-command \
        --region "$REGION" \
        --instance-ids "$instance_id" \
        --document-name "AWS-RunShellScript" \
        --parameters 'commands=["tail -3 /var/log/ami-build.log 2>/dev/null || echo waiting..."]' \
        --query 'Command.CommandId' --output text 2>/dev/null) || true
      if [ -n "$cmd_id" ]; then
        sleep 2
        log_lines=$(aws ssm get-command-invocation \
          --region "$REGION" \
          --command-id "$cmd_id" \
          --instance-id "$instance_id" \
          --query 'StandardOutputContent' --output text 2>/dev/null | head -3) || true
      fi
    fi

    echo "[$i/$timeout] Build status: $status"
    if [ -n "$log_lines" ]; then
      echo "$log_lines" | sed 's/^/  > /'
    fi

    if [ "$status" = "complete" ]; then
      return 0
    fi
    sleep 28  # 2s already spent on SSM
  done
  echo "Build timeout!"
  return 1
}

# Main
main() {
  local hash
  hash=$(compute_hash)
  echo "Build hash: $hash"

  # Check cache
  existing=$(check_existing_ami "$hash")
  if [ "$existing" != "None" ] && [ -n "$existing" ]; then
    echo "CACHED: $existing"
    echo "ami_id=$existing" >> "${GITHUB_OUTPUT:-/dev/null}"
    echo "cached=true" >> "${GITHUB_OUTPUT:-/dev/null}"
    exit 0
  fi

  echo "No cached AMI, building..."

  # Clean up any orphaned builder instances (from cancelled runs)
  orphans=$(aws ec2 describe-instances \
    --region "$REGION" \
    --filters "Name=tag:Name,Values=ami-builder-temp" "Name=instance-state-name,Values=running,pending" \
    --query 'Reservations[].Instances[].InstanceId' --output text)
  if [ -n "$orphans" ]; then
    echo "Cleaning up orphaned instances: $orphans"
    aws ec2 terminate-instances --region "$REGION" --instance-ids $orphans || true
  fi

  # Get base AMI
  base_ami=$(get_base_ami)
  echo "Base AMI: $base_ami"

  # Create user data
  user_data_file=$(mktemp)
  create_user_data > "$user_data_file"

  # Launch instance (AWS CLI base64-encodes file:// automatically)
  instance_id=$(aws ec2 run-instances \
    --region "$REGION" \
    --image-id "$base_ami" \
    --instance-type c7g.2xlarge \
    --subnet-id subnet-05c215519b2150ecd \
    --security-group-ids sg-0ebf2d8c6a0acc1a3 \
    --iam-instance-profile Name=jumpbox-admin-profile \
    --associate-public-ip-address \
    --block-device-mappings '[{"DeviceName":"/dev/sda1","Ebs":{"VolumeSize":100,"VolumeType":"gp3","DeleteOnTermination":true}}]' \
    --tag-specifications 'ResourceType=instance,Tags=[{Key=Name,Value=ami-builder-temp},{Key=BuildStatus,Value=starting}]' \
    --user-data "file://$user_data_file" \
    --query 'Instances[0].InstanceId' \
    --output text)
  echo "Launched instance: $instance_id"

  # Cleanup function
  cleanup() {
    echo "Cleaning up instance $instance_id..."
    aws ec2 terminate-instances --region "$REGION" --instance-ids "$instance_id" || true
  }
  trap cleanup EXIT

  # Wait for build
  if ! wait_for_build "$instance_id"; then
    echo "Build failed!"
    exit 1
  fi

  # Stop instance for AMI creation
  echo "Stopping instance..."
  aws ec2 stop-instances --region "$REGION" --instance-ids "$instance_id"
  aws ec2 wait instance-stopped --region "$REGION" --instance-ids "$instance_id"

  # Get kernel version from instance tags
  kernel_version=$(aws ec2 describe-tags \
    --region "$REGION" \
    --filters "Name=resource-id,Values=$instance_id" "Name=key,Values=KernelVersion" \
    --query 'Tags[0].Value' --output text 2>/dev/null || echo "unknown")

  # Create AMI
  timestamp=$(date +%Y%m%d-%H%M)
  ami_name="firepod-runner-${kernel_version}-${timestamp}"

  ami_id=$(aws ec2 create-image \
    --region "$REGION" \
    --instance-id "$instance_id" \
    --name "$ami_name" \
    --description "Firepod CI runner with kernel ${kernel_version}-nested" \
    --query 'ImageId' --output text)
  echo "Created AMI: $ami_id ($ami_name)"

  # Wait for AMI (custom loop - default waiter times out on large disks)
  echo "Waiting for AMI to be available..."
  for i in $(seq 1 60); do  # 60 * 30s = 30 min max
    state=$(aws ec2 describe-images --region "$REGION" --image-ids "$ami_id" --query 'Images[0].State' --output text)
    echo "[$i/60] AMI state: $state"
    if [ "$state" = "available" ]; then
      break
    elif [ "$state" = "failed" ]; then
      echo "AMI creation failed!"
      exit 1
    fi
    sleep 30
  done

  # Tag AMI
  aws ec2 create-tags --region "$REGION" --resources "$ami_id" --tags \
    Key=Name,Value="$ami_name" \
    Key=Kernel,Value="${kernel_version}-nested" \
    Key=BuildHash,Value="$hash" \
    Key=Purpose,Value=github-runner

  echo "SUCCESS: $ami_id"
  echo "ami_id=$ami_id" >> "${GITHUB_OUTPUT:-/dev/null}"
  echo "kernel_version=$kernel_version" >> "${GITHUB_OUTPUT:-/dev/null}"
  echo "cached=false" >> "${GITHUB_OUTPUT:-/dev/null}"
}

main "$@"
