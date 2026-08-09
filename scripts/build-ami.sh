#!/bin/bash
# Build GitHub runner AMI with custom kernel
# Called from CI workflow - requires AWS credentials
set -euo pipefail

REGION="${AWS_REGION:-us-west-1}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
KERNEL_DIR="$(dirname "$SCRIPT_DIR")/kernel"

# Materialize exactly the Git tree the AMI builder will fetch. Hashing the
# caller's working-tree bytes and pinning only HEAD can otherwise label an AMI
# with a key for dirty/smudged files that never reach the builder. The running
# provisioning script is checked too: it defines create_user_data(), so it must
# be byte-identical to the pinned tree before its output can enter the key.
materialize_pinned_source_tree() {
  local repo_root="$1"
  local source_commit="$2"
  local destination="$3"

  if ! git -C "$repo_root" archive --format=tar "$source_commit" | \
    tar -xf - -C "$destination"; then
    echo "ERROR: cannot materialize fcvm source commit $source_commit" >&2
    return 1
  fi
  if ! cmp -s -- \
    "$repo_root/scripts/build-ami.sh" \
    "$destination/scripts/build-ami.sh"; then
    echo "ERROR: scripts/build-ami.sh differs from pinned commit $source_commit;" >&2
    echo "       refusing to hash bytes the AMI builder will not provision." >&2
    return 1
  fi
}

# Compute from the immutable tree the remote builder will fetch. Keeping the
# materialization and KERNEL_DIR/SCRIPT_DIR rebinding in one function makes it
# behaviorally testable and prevents main from accidentally hashing the caller
# worktree again during a later refactor.
compute_pinned_hash() {
  local repo_root="$1"
  local source_commit="$2"
  local source_tree hash

  source_tree="$(mktemp -d)" || return 1
  if ! materialize_pinned_source_tree "$repo_root" "$source_commit" "$source_tree"; then
    rm -rf -- "$source_tree"
    return 1
  fi

  # Bash functions resolve dynamically scoped locals, so compute_hash and
  # create_user_data read only the archived commit.
  local KERNEL_DIR="$source_tree/kernel"
  local SCRIPT_DIR="$source_tree/scripts"
  if ! hash=$(compute_hash "$source_commit"); then
    rm -rf -- "$source_tree"
    return 1
  fi
  rm -rf -- "$source_tree"
  printf '%s\n' "$hash"
}

# Compute build hash (from kernel config + patches + boot_args + passt build inputs)
compute_hash() {
  local source_commit="$1"
  local repo_root
  repo_root="$(dirname "$KERNEL_DIR")"

  if [[ ! $source_commit =~ ^[0-9a-f]{40}$ ]]; then
    echo "ERROR: invalid fcvm source commit for AMI cache key: $source_commit" >&2
    return 1
  fi

  # FAIL CLOSED. Every read below used to be `2>/dev/null` with its failure
  # discarded, and `hash=$(compute_hash)` does NOT inherit errexit inside the
  # command substitution — so a missing glob, an unreadable patch or a dangling
  # symlink produced a valid-looking hash computed over FEWER inputs. That is the
  # stale-AMI bug this function exists to prevent, wearing a different hat:
  # check_existing_ami would match an image built from a different kernel.
  local -a host_patches=()
  local p
  shopt -s nullglob
  for p in "$KERNEL_DIR/patches-arm64/"*.patch; do
    case "$p" in *.vm.patch) continue ;; esac
    host_patches+=("$p")
  done
  shopt -u nullglob
  if [ ${#host_patches[@]} -eq 0 ]; then
    echo "ERROR: no host-kernel patches matched $KERNEL_DIR/patches-arm64/*.patch;" >&2
    echo "       the AMI cache key would silently omit them." >&2
    return 1
  fi

  # Only the host kernel this AMI bakes. rootfs-config.toml carries eight
  # kernel_version assignments, so a bare grep also invalidated the key whenever
  # an unrelated profile (btrfs.arm64, nested.amd64, ...) changed, forcing a full
  # EC2 rebuild. The builder always selects [kernel_profiles.nested.arm64.host_kernel]
  # on ARM64. Resolved HERE, not inside the `{ ... } | sha256sum` group below,
  # because `exit` inside that group leaves only the subshell — sha256sum would
  # still hash the partial input and emit a plausible key.
  local host_kernel_version
  if ! host_kernel_version=$(awk '
      /^\[/ { in_section = ($0 == "[kernel_profiles.nested.arm64.host_kernel]") }
      in_section && /^kernel_version[[:space:]]*=/ { print; found = 1 }
      END { exit(found ? 0 : 1) }
    ' "$repo_root/rootfs-config.toml"); then
    echo "ERROR: [kernel_profiles.nested.arm64.host_kernel] has no kernel_version;" >&2
    echo "       the AMI cache key cannot identify the host kernel." >&2
    return 1
  fi

  local f
  for f in "$KERNEL_DIR/nested.conf" "$repo_root/rootfs-config.toml" \
    "$SCRIPT_DIR/build-passt.sh" "$SCRIPT_DIR/install-runner-disk-guard.sh" \
    "$SCRIPT_DIR/runner-disk-preflight.sh" \
    "$SCRIPT_DIR/prune-cargo-target.sh" \
    "$SCRIPT_DIR/runner-disk-guard.service" "$SCRIPT_DIR/runner-disk-guard.timer" \
    "${host_patches[@]}"; do
    if [ ! -r "$f" ]; then
      echo "ERROR: AMI cache key input is missing or unreadable: $f" >&2
      return 1
    fi
  done

  # Frame every baked disk-guard input independently. Raw concatenation makes
  # `ab` + `cd` indistinguishable from `abc` + `d`, and a failed cat inside the
  # larger hash pipeline can be masked by a later successful command. sha256sum
  # emits a name-tagged digest for each file and fails before any cache key is
  # produced if a read fails.
  local disk_guard_digests
  if ! disk_guard_digests=$(
    cd "$repo_root" &&
      sha256sum \
        scripts/runner-disk-preflight.sh \
        scripts/prune-cargo-target.sh \
        scripts/install-runner-disk-guard.sh \
        scripts/runner-disk-guard.service \
        scripts/runner-disk-guard.timer
  ); then
    echo "ERROR: cannot digest every disk-guard AMI input" >&2
    return 1
  fi

  {
    cat "$KERNEL_DIR/nested.conf"
    # The HOST kernel baked into this AMI is built from the arm64 patch set —
    # [kernel_profiles.nested.arm64.host_kernel] build_inputs is
    # "kernel/patches-arm64/*.patch" — NOT kernel/patches. Only two of those nine
    # are symlinks into kernel/patches; the other seven (nv2-vsock-cache-sync,
    # nv2-vsock-rx-barrier, wfx-stopped-exit, the psci-debug set) were invisible
    # to this hash, so changing one produced an identical AMI hash and the
    # builder reused a stale image carrying the OLD host kernel.
    # `.vm.patch` is excluded to match compute_host_kernel_sha in
    # src/setup/kernel.rs, which applies those only to the guest kernel.
    cat "${host_patches[@]}"
    # kernel_version is part of the host kernel's identity but was never read,
    # so a pure version bump left the AMI hash unchanged.
    printf '%s\n' "$host_kernel_version"
    # Include boot_args from config to invalidate cache when they change
    grep -E '^boot_args\s*=' "$repo_root/rootfs-config.toml" 2>/dev/null || true
    # Include the passt build inputs so a pin or patch change rebuilds the AMI
    # instead of reusing one whose baked-in pasta no longer matches CI.
    cat "$SCRIPT_DIR/build-passt.sh" "$SCRIPT_DIR"/passt-*.patch 2>/dev/null
    # Include the disk guard's script and units: they are baked into the AMI,
    # so a change to them must produce a new AMI rather than silently reusing
    # one without the fix.
    printf '%s\n' "$disk_guard_digests"
    # Include the exact rendered provisioning script, including the immutable
    # source commit it fetches and compiles. Hashing a placeholder commit let a
    # host-tool change outside the selective inputs above reuse an AMI built
    # from an older checkout even though a cache miss would build the new one.
    create_user_data "$source_commit"
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
  local source_commit="$1"
  if [[ ! $source_commit =~ ^[0-9a-f]{40}$ ]]; then
    echo "ERROR: invalid fcvm source commit for AMI user data: $source_commit" >&2
    return 1
  fi
  sed "s/__FCVM_SOURCE_COMMIT__/$source_commit/g" << 'USERDATA'
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
  podman uidmap passt fuse-overlayfs containernetworking-plugins \
  fuse3 libfuse3-dev libclang-dev clang musl-tools \
  iproute2 iptables dnsmasq qemu-utils e2fsprogs parted \
  skopeo busybox-static cpio zstd autoconf automake libtool \
  nfs-kernel-server libseccomp-dev python3 util-linux

# Node.js 22.x
curl -fsSL https://deb.nodesource.com/setup_22.x | bash -
apt-get install -y nodejs

# Rust belongs to the runner user. The Makefile deliberately rejects host
# builds as root because they leave root-owned target artifacts behind.
sudo -u ubuntu env HOME=/home/ubuntu bash -c \
  'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y'

# Fetch the exact checkout whose inputs produced this AMI's cache key. Cloning
# moving main here allowed a merge during instance boot to install a different
# disk-guard protocol than the one compute_hash read.
FCVM_SOURCE_COMMIT="__FCVM_SOURCE_COMMIT__"
git init /tmp/fcvm
git -C /tmp/fcvm remote add origin https://github.com/ejc3/fcvm.git
git -C /tmp/fcvm fetch --depth 1 origin "$FCVM_SOURCE_COMMIT"
git -C /tmp/fcvm checkout --detach FETCH_HEAD
git clone --depth 1 https://github.com/ejc3/fuse-backend-rs.git /tmp/fuse-backend-rs
git clone --depth 1 https://github.com/ejc3/fuser.git /tmp/fuser
chown -R ubuntu:ubuntu /tmp/fcvm /tmp/fuse-backend-rs /tmp/fuser
sudo -u ubuntu env HOME=/home/ubuntu bash -c \
  'source "$HOME/.cargo/env"; cd /tmp/fcvm; make build-host-tools'
cd /tmp/fcvm

# Build pasta/passt from the repo's pinned source so the AMI matches CI
# (scripts/build-passt.sh owns the pin and the local patches).
./scripts/build-passt.sh

# Use repo's config which has nested profile defined
mkdir -p /root/.config/fcvm
cp rootfs-config.toml /root/.config/fcvm/

# Build and install kernel using fcvm setup
aws ec2 create-tags --resources $INSTANCE_ID --tags Key=KernelVersion,Value=nested --region us-west-1
./target/release/fcvm setup --kernel-profile nested --build-kernels --install-host-kernel

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

# Disk-capacity guard (hourly timer). A runner that fills its disk fails every
# job it picks up at "Set up job", before any job step can run, so the CI
# preflight step cannot rescue it — this cleans caches out-of-band and, if the
# hard floor still cannot be met, stops the runner service so the box goes
# offline and gets replaced instead of poisoning jobs.
/tmp/fcvm/scripts/install-runner-disk-guard.sh /tmp/fcvm
systemctl enable runner-disk-guard.timer

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
  local hash repo_root source_commit
  repo_root="$(dirname "$KERNEL_DIR")"
  if ! source_commit="$(git -C "$repo_root" rev-parse HEAD)" || \
    [[ ! $source_commit =~ ^[0-9a-f]{40}$ ]]; then
    echo "ERROR: cannot identify the exact fcvm source commit for AMI provisioning" >&2
    exit 1
  fi

  if ! hash=$(compute_pinned_hash "$repo_root" "$source_commit"); then
    echo "ERROR: cannot compute the AMI cache key; refusing to reuse or tag an AMI" >&2
    exit 1
  fi
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
  create_user_data "$source_commit" > "$user_data_file"

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
  ami_name="fcvm-runner-${kernel_version}-${timestamp}"

  ami_id=$(aws ec2 create-image \
    --region "$REGION" \
    --instance-id "$instance_id" \
    --name "$ami_name" \
    --description "fcvm CI runner with kernel ${kernel_version}-nested" \
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
