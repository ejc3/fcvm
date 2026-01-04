#!/bin/bash
# Build GitHub runner AMI with custom kernel
# Called from CI workflow - requires AWS credentials
set -euo pipefail

REGION="${AWS_REGION:-us-west-1}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
KERNEL_DIR="$(dirname "$SCRIPT_DIR")/kernel"

# Compute build hash (from kernel config + patches + boot_args)
compute_hash() {
  local repo_root="$(dirname "$KERNEL_DIR")"
  {
    cat "$KERNEL_DIR/nested.conf" "$KERNEL_DIR/patches/"*.patch 2>/dev/null
    # Include boot_args from config to invalidate cache when they change
    grep -E '^boot_args\s*=' "$repo_root/rootfs-config.toml" 2>/dev/null || true
  } | sha256sum | cut -c1-12
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

# SSH keys: fcvm-ec2 + dev servers can SSH in for debugging
mkdir -p /home/ubuntu/.ssh
chmod 700 /home/ubuntu/.ssh
# Static keys (fcvm-ec2 for jumpbox, dev-to-runner for dev servers)
cat >> /home/ubuntu/.ssh/authorized_keys << 'SSHKEYS'
ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINwtXjjTCVgT9OR3qrnz3zDkV2GveuCBlWFXSOBG2joe fcvm-ec2
ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPEnsYFangbzY7I0yUxa1sr0MNWN9fMiAKIcUpV6KaLn dev-to-runner
SSHKEYS
chmod 600 /home/ubuntu/.ssh/authorized_keys
chown -R ubuntu:ubuntu /home/ubuntu/.ssh

# Setup NVMe instance storage (find NVMe that isn't the root disk)
ROOT_DEV=$(lsblk -no PKNAME $(findmnt -no SOURCE /) | head -1)
NVME_DEV=$(lsblk -dn -o NAME,TYPE | awk '$2=="disk" && /^nvme/ {print $1}' | grep -v "^$ROOT_DEV$" | head -1)
if [ -n "$NVME_DEV" ]; then
  echo "Setting up NVMe: /dev/$NVME_DEV"
  mkfs.ext4 -F /dev/$NVME_DEV
  mount /dev/$NVME_DEV /tmp
  chmod 1777 /tmp
else
  echo "WARNING: No NVMe found, using EBS for builds"
fi

# Install deps (xz-utils needed for kernel kheaders tarball)
apt-get install -y build-essential bc bison flex libssl-dev \
  libelf-dev libncurses-dev libdw-dev debhelper-compat rsync kmod cpio curl jq wget git \
  dwarves xz-utils \
  podman uidmap slirp4netns fuse-overlayfs containernetworking-plugins \
  fuse3 libfuse3-dev libclang-dev clang musl-tools \
  iproute2 iptables dnsmasq qemu-utils e2fsprogs parted \
  skopeo busybox-static cpio zstd autoconf automake libtool \
  nfs-kernel-server libseccomp-dev

# Node.js 22.x
curl -fsSL https://deb.nodesource.com/setup_22.x | bash -
apt-get install -y nodejs

# Rust (set HOME for cloud-init context)
export HOME=/root
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source /root/.cargo/env

# Clone firepod and its dependencies
git clone --depth 1 https://github.com/ejc3/firepod.git /tmp/firepod
git clone --depth 1 https://github.com/ejc3/fuse-backend-rs.git /tmp/fuse-backend-rs
git clone --depth 1 https://github.com/ejc3/fuser.git /tmp/fuser
cd /tmp/firepod
cargo build --release

# Use repo's config which has nested profile defined
mkdir -p /root/.config/fcvm
cp rootfs-config.toml /root/.config/fcvm/

# Build and install kernel using fcvm setup
aws ec2 create-tags --resources $INSTANCE_ID --tags Key=KernelVersion,Value=nested --region us-west-1
./target/release/fcvm setup --kernel-profile nested --build-kernels --install-host-kernel

# Rust for ubuntu user (separate from root's rust used for build)
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
      echo "================================================"
      echo "BUILD FAILED - Fetching error log..."
      echo "================================================"
      # Try to get error context via SSM (wait longer for command)
      cmd_id=$(aws ssm send-command \
        --region "$REGION" \
        --instance-ids "$instance_id" \
        --document-name "AWS-RunShellScript" \
        --parameters 'commands=["tail -100 /var/log/ami-build.log"]' \
        --query 'Command.CommandId' --output text) || true
      if [ -n "$cmd_id" ]; then
        # Wait for command to complete
        for j in $(seq 1 10); do
          sleep 2
          cmd_status=$(aws ssm get-command-invocation \
            --region "$REGION" \
            --command-id "$cmd_id" \
            --instance-id "$instance_id" \
            --query 'Status' --output text 2>/dev/null) || true
          if [ "$cmd_status" = "Success" ] || [ "$cmd_status" = "Failed" ]; then
            break
          fi
        done
        # Print the log output
        echo "--- Build Log (last 100 lines) ---"
        aws ssm get-command-invocation \
          --region "$REGION" \
          --command-id "$cmd_id" \
          --instance-id "$instance_id" \
          --query 'StandardOutputContent' --output text || echo "Could not fetch log"
        echo "--- End Log ---"
      else
        echo "Could not send SSM command"
      fi
      return 1
    fi

    # Show progress
    echo "[$i/$timeout] Build status: $status (instance: $instance_id)"
    if [ "$status" = "building" ]; then
      # Try SSM first
      echo "  Fetching logs via SSM..."
      cmd_id=$(aws ssm send-command \
        --region "$REGION" \
        --instance-ids "$instance_id" \
        --document-name "AWS-RunShellScript" \
        --parameters 'commands=["tail -15 /var/log/ami-build.log"]' \
        --query 'Command.CommandId' --output text 2>&1)
      echo "  SSM command: $cmd_id"
      if [ -n "$cmd_id" ] && [[ ! "$cmd_id" =~ "error" ]]; then
        sleep 5
        echo "  Getting SSM output..."
        aws ssm get-command-invocation \
          --region "$REGION" \
          --command-id "$cmd_id" \
          --instance-id "$instance_id" \
          --output text 2>&1 | head -20
      fi
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

  # Launch instance - try spot first, fall back to on-demand
  echo "Trying spot instance..."
  instance_id=$(aws ec2 run-instances \
    --region "$REGION" \
    --image-id "$base_ami" \
    --instance-type c7gd.8xlarge \
    --instance-market-options '{"MarketType":"spot"}' \
    --subnet-id subnet-05c215519b2150ecd \
    --security-group-ids sg-0ebf2d8c6a0acc1a3 \
    --iam-instance-profile Name=jumpbox-admin-profile \
    --associate-public-ip-address \
    --block-device-mappings '[{"DeviceName":"/dev/sda1","Ebs":{"VolumeSize":40,"VolumeType":"gp3","DeleteOnTermination":true}}]' \
    --tag-specifications 'ResourceType=instance,Tags=[{Key=Name,Value=ami-builder-temp},{Key=BuildStatus,Value=starting}]' \
    --user-data "file://$user_data_file" \
    --query 'Instances[0].InstanceId' \
    --output text 2>&1) || true

  # Fall back to on-demand if spot fails
  if [[ -z "$instance_id" ]] || [[ "$instance_id" == *"error"* ]] || [[ "$instance_id" == *"Error"* ]]; then
    echo "Spot failed, using on-demand..."
    instance_id=$(aws ec2 run-instances \
      --region "$REGION" \
      --image-id "$base_ami" \
      --instance-type c7gd.8xlarge \
      --subnet-id subnet-05c215519b2150ecd \
      --security-group-ids sg-0ebf2d8c6a0acc1a3 \
      --iam-instance-profile Name=jumpbox-admin-profile \
      --associate-public-ip-address \
      --block-device-mappings '[{"DeviceName":"/dev/sda1","Ebs":{"VolumeSize":40,"VolumeType":"gp3","DeleteOnTermination":true}}]' \
      --tag-specifications 'ResourceType=instance,Tags=[{Key=Name,Value=ami-builder-temp},{Key=BuildStatus,Value=starting}]' \
      --user-data "file://$user_data_file" \
      --query 'Instances[0].InstanceId' \
      --output text)
  fi
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
