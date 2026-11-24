#!/bin/bash
# Build GitHub runner AMI with custom kernel
# Called from CI workflow - requires AWS credentials
#
# AMI_ARCH selects the image: arm64 (the default) or x64. Both install the host kernel of
# their [kernel_profiles.nested.<arch>.host_kernel] table.
set -euo pipefail

REGION="${AWS_REGION:-us-west-1}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
KERNEL_DIR="$(dirname "$SCRIPT_DIR")/kernel"
FCVM_SOURCE_REMOTE="${FCVM_SOURCE_REMOTE:-https://github.com/ejc3/fcvm.git}"

# Canonical's name for the architecture in its image names, EC2's name for it, the
# builder's instance type (each has local NVMe for the kernel build) and the type the
# candidate image is boot-tested on: bare metal of a family the runner launcher uses.
#
# The builder role (ejc3/aws github-ami-builder.tf) decides what this script may launch,
# tag and publish. It allowed one type, c7gd.8xlarge, and images created already tagged
# Purpose=github-runner; the x64 leg and the candidate step below need the grants of
# ejc3/aws#1031. Until that is applied a run fails closed, publishing nothing.
AMI_ARCH="${AMI_ARCH:-arm64}"
case "$AMI_ARCH" in
  arm64) UBUNTU_ARCH=arm64 EC2_ARCH=arm64 BUILDER_TYPE=c7gd.8xlarge BOOT_TEST_TYPE=c7gd.metal ;;
  x64) UBUNTU_ARCH=amd64 EC2_ARCH=x86_64 BUILDER_TYPE=c5d.9xlarge BOOT_TEST_TYPE=c5d.metal ;;
  *)
    echo "ERROR: AMI_ARCH must be arm64 or x64, got '$AMI_ARCH'" >&2
    exit 1
    ;;
esac

# Prove the cloud builder can perform the exact fetch rendered below before an
# EC2 instance is allocated. A local HEAD can identify a perfectly valid but
# unpushed commit; materializing it locally is not evidence that the separate
# builder can fetch it. Use a fresh object database so the local checkout's
# existing object cannot make this probe pass vacuously.
verify_source_commit_fetchable() {
  local repo_root="$1"
  local source_commit="$2"
  local source_remote="$3"
  local fetch_probe

  if [[ ! $source_commit =~ ^[0-9a-f]{40}$ ]] || \
    ! git -C "$repo_root" cat-file -e "$source_commit^{commit}"; then
    echo "ERROR: invalid local fcvm source commit: $source_commit" >&2
    return 1
  fi

  fetch_probe="$(mktemp -d)" || return 1
  if ! git -C "$fetch_probe" init --quiet || \
    ! git -C "$fetch_probe" fetch --quiet --depth 1 \
      "$source_remote" "$source_commit"; then
    rm -rf -- "$fetch_probe"
    echo "ERROR: fcvm source commit $source_commit is not fetchable from $source_remote;" >&2
    echo "       push the commit before launching the AMI builder." >&2
    return 1
  fi
  rm -rf -- "$fetch_probe"
}

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
  # The image's architecture picks the host kernel table and the nested guest config.
  # It is part of the key, so the two images never share one.
  local ami_arch="${AMI_ARCH:-arm64}" host_section nested_conf
  case "$ami_arch" in
    arm64) host_section="[kernel_profiles.nested.arm64.host_kernel]" nested_conf=nested.conf ;;
    x64) host_section="[kernel_profiles.nested.amd64.host_kernel]" nested_conf=nested-x86.conf ;;
    *)
      echo "ERROR: AMI_ARCH must be arm64 or x64, got '$ami_arch'" >&2
      return 1
      ;;
  esac

  # The host kernel's patch set is the patches_dir its own table names, which can
  # differ from the guest's (#659 ran a 7.3 host from its own set under 6.18 guests),
  # so read it from that table rather than assume a directory.
  local host_patches_dir
  if ! host_patches_dir=$(awk -v section="$host_section" '
      /^\[/ { in_section = ($0 == section) }
      in_section && /^patches_dir[[:space:]]*=/ {
        v = $0; sub(/^[^=]*=[[:space:]]*"/, "", v); sub(/".*$/, "", v); print v; found = 1
      }
      END { exit(found ? 0 : 1) }
    ' "$repo_root/rootfs-config.toml") || [ -z "$host_patches_dir" ]; then
    echo "ERROR: $host_section has no patches_dir;" >&2
    echo "       the AMI cache key cannot find the host kernel's patches." >&2
    return 1
  fi

  local -a host_patches=()
  local p
  shopt -s nullglob
  for p in "$repo_root/$host_patches_dir/"*.patch; do
    case "$p" in *.vm.patch) continue ;; esac
    host_patches+=("$p")
  done
  shopt -u nullglob
  if [ ${#host_patches[@]} -eq 0 ]; then
    echo "ERROR: no host-kernel patches matched $repo_root/$host_patches_dir/*.patch;" >&2
    echo "       the AMI cache key would silently omit them." >&2
    return 1
  fi

  # Only the host kernel this AMI bakes. rootfs-config.toml carries many
  # kernel_version assignments, so a bare grep also invalidated the key whenever
  # an unrelated profile (btrfs.arm64, nested.amd64, ...) changed, forcing a full
  # EC2 rebuild. The builder selects the host_kernel table of its own architecture.
  # Resolved HERE, not inside the `{ ... } | sha256sum` group below,
  # because `exit` inside that group leaves only the subshell — sha256sum would
  # still hash the partial input and emit a plausible key.
  local host_kernel_version
  if ! host_kernel_version=$(awk -v section="$host_section" '
      /^\[/ { in_section = ($0 == section) }
      in_section && /^kernel_version[[:space:]]*=/ { print; found = 1 }
      END { exit(found ? 0 : 1) }
    ' "$repo_root/rootfs-config.toml"); then
    echo "ERROR: $host_section has no kernel_version;" >&2
    echo "       the AMI cache key cannot identify the host kernel." >&2
    return 1
  fi

  local f
  for f in "$KERNEL_DIR/$nested_conf" "$repo_root/rootfs-config.toml" \
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
    cd "$SCRIPT_DIR" &&
      sha256sum \
        runner-disk-preflight.sh \
        prune-cargo-target.sh \
        install-runner-disk-guard.sh \
        runner-disk-guard.service \
        runner-disk-guard.timer
  ); then
    echo "ERROR: cannot digest every disk-guard AMI input" >&2
    return 1
  fi

  {
    printf 'arch=%s\n' "$ami_arch"
    cat "$KERNEL_DIR/$nested_conf"
    # The HOST kernel baked into this AMI is built from the patches_dir of
    # its host_kernel table, NOT kernel/patches. Most of the files
    # there were never in kernel/patches (the NV2 and debug patches of the time),
    # so they were invisible to this hash, so changing one produced an identical AMI hash and the
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

# Check for an existing PUBLISHED image of this architecture with a matching hash. A
# candidate that was never boot-tested, or one that failed its test, carries the same
# BuildHash and another Purpose: reusing it would publish what the boot test exists to stop.
check_existing_ami() {
  local hash="$1"
  aws ec2 describe-images \
    --region "$REGION" \
    --owners self \
    --filters "Name=tag:BuildHash,Values=$hash" "Name=state,Values=available" \
    "Name=tag:Purpose,Values=github-runner" "Name=architecture,Values=$EC2_ARCH" \
    --query 'Images[0].ImageId' --output text
}

# Get latest Ubuntu 24.04 AMI of the image's architecture
get_base_ami() {
  aws ec2 describe-images \
    --region "$REGION" \
    --owners 099720109477 \
    --filters "Name=name,Values=ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-${UBUNTU_ARCH}-server-*" \
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
  sed \
    -e "s/__FCVM_SOURCE_COMMIT__/$source_commit/g" \
    -e "s#__FCVM_SOURCE_REMOTE__#$FCVM_SOURCE_REMOTE#g" << 'USERDATA'
#!/bin/bash
exec > >(tee /var/log/ami-build.log) 2>&1
set -Eeuo pipefail

# Why a build failed, through the one channel this host may write: its BuildStatus tag.
# CI cannot read this log, so the handler puts the failing line and command in the tag's
# value. It is installed before anything can fail (-E carries it into functions). Until
# the AWS CLI below is installed there is nothing to tag with, so a failure in that window
# only stops the script. The window holds the two metadata requests, `apt-get update`,
# `apt-get install -y unzip curl` and the CLI download and install; wait_for_build fails a
# builder still "starting" after 20 minutes, which the bounds below stay inside.
INSTANCE_ID=unknown
tag_failed() {
  local reason
  trap - ERR
  echo "BUILD FAILED at line $1: $2"
  reason=$(printf 'failed:line %s:%s' "$1" "$2" | tr -c 'A-Za-z0-9 _.:/=+@-' '_' | cut -c1-200)
  if command -v aws > /dev/null 2>&1; then
    aws ec2 create-tags --resources "$INSTANCE_ID" --region us-west-1 \
      --tags "[{\"Key\":\"BuildStatus\",\"Value\":\"$reason\"}]" || true
  fi
  exit 1
}
trap 'tag_failed "$LINENO" "$BASH_COMMAND"' ERR

# Every apt call waits for the dpkg frontend lock. `apt-get update` starts Ubuntu Pro's
# esm-cache.service, whose own apt holds that lock for some seconds afterwards, and an apt
# call that meets a held lock fails at once. On 2026-11-24 the x64 builder's
# `apt-get install -y unzip curl` met it 27 s after boot ("Could not get lock
# /var/lib/dpkg/lock-frontend. It is held by process N (apt)"), before anything could be
# tagged, and the arm64 builder stopped one apt call later. The apt setting covers the
# apt calls inside the NodeSource and runner dependency scripts too, and is removed at
# the end, so the image keeps apt's defaults. apt_get() also retries downloads and runs a
# failed call again: DPkg::Lock::Timeout does not cover the lists lock `update` takes.
#
# The bounds: a call waits 120 s for the lock and is made 3 times, 10 s apart, so one
# that only ever meets a held lock gives up after 3 x 120 + 2 x 10 = 380 s. The two
# calls before the "building" tag can therefore take 760 s, and with the boot, the CLI
# download's retries (about 30 s of back-off) and its install that stays under the
# 1200 s wait_for_build allows a builder that has not tagged itself.
export DEBIAN_FRONTEND=noninteractive
echo 'DPkg::Lock::Timeout "120";' > /etc/apt/apt.conf.d/99-fcvm-builder-lock-wait
apt_get() {
  local attempt
  for attempt in 1 2 3; do
    if apt-get -o DPkg::Lock::Timeout=120 -o Acquire::Retries=5 "$@"; then
      return 0
    fi
    echo "apt-get $* failed (attempt $attempt of 3)"
    sleep 10
  done
  # Name the call that failed and its line in this script, not this function's `return`.
  tag_failed "${BASH_LINENO[0]}" "apt-get $*"
}

# dpkg run directly (the host kernel install calls `dpkg -i`) takes no apt option, so it
# does not wait for the frontend lock. Wait here, for at most the given seconds, until no
# process holds it. apt and dpkg lock that file with fcntl, which flock(1) does not see.
wait_for_dpkg_lock() {
  python3 - "${1:-/var/lib/dpkg/lock-frontend}" "${2:-120}" << 'PY'
import fcntl, sys, time
path, deadline = sys.argv[1], time.monotonic() + float(sys.argv[2])
while True:
    with open(path, "a") as lock:
        try:
            fcntl.lockf(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            sys.exit(0)
        except OSError:
            pass
    if time.monotonic() >= deadline:
        sys.exit("the dpkg frontend lock at %s is still held" % path)
    time.sleep(2)
PY
}

# Get IMDSv2 token and instance ID. Tracing starts after the token's last use: under
# `set -x` the token would be written to this log and to the instance's console.
TOKEN=$(curl -X PUT "http://169.254.169.254/latest/api/token" -H "X-aws-ec2-metadata-token-ttl-seconds: 21600" -s)
INSTANCE_ID=$(curl -H "X-aws-ec2-metadata-token: $TOKEN" -s http://169.254.169.254/latest/meta-data/instance-id)
unset TOKEN
set -x

# One script provisions both architectures: the download names follow the machine.
MACHINE=$(uname -m)
case "$MACHINE" in
  aarch64) RUNNER_ARCH=arm64 ;;
  x86_64) RUNNER_ARCH=x64 ;;
esac

# Install AWS CLI first (needed for tagging). A failed list fetch is an error here, so it
# is retried instead of leaving `install` without a package list.
apt_get -o APT::Update::Error-Mode=any update
apt_get install -y unzip curl
curl --fail --retry 5 --retry-all-errors "https://awscli.amazonaws.com/awscli-exe-linux-${MACHINE}.zip" -o "/tmp/awscliv2.zip"
unzip -q /tmp/awscliv2.zip -d /tmp
/tmp/aws/install

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
apt_get install -y build-essential bc bison flex libssl-dev \
  libelf-dev libncurses-dev libdw-dev debhelper-compat rsync kmod cpio curl jq wget git \
  dwarves xz-utils \
  podman uidmap passt fuse-overlayfs containernetworking-plugins \
  fuse3 libfuse3-dev libclang-dev clang musl-tools \
  iproute2 iptables dnsmasq qemu-utils e2fsprogs parted \
  skopeo busybox-static cpio zstd autoconf automake libtool \
  nfs-kernel-server libseccomp-dev python3 util-linux

# Node.js 22.x
curl -fsSL https://deb.nodesource.com/setup_22.x | bash -
apt_get install -y nodejs

# Rust belongs to the runner user. The Makefile deliberately rejects host
# builds as root because they leave root-owned target artifacts behind.
sudo -u ubuntu env HOME=/home/ubuntu bash -c \
  'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y'

# Fetch the exact checkout whose inputs produced this AMI's cache key. Cloning
# moving main here allowed a merge during instance boot to install a different
# disk-guard protocol than the one compute_hash read.
FCVM_SOURCE_COMMIT="__FCVM_SOURCE_COMMIT__"
FCVM_SOURCE_REMOTE="__FCVM_SOURCE_REMOTE__"
git init /tmp/fcvm
git -C /tmp/fcvm remote add origin "$FCVM_SOURCE_REMOTE"
git -C /tmp/fcvm fetch --depth 1 origin "$FCVM_SOURCE_COMMIT"
git -C /tmp/fcvm checkout --detach FETCH_HEAD
git clone --depth 1 https://github.com/ejc3/fuse-backend-rs.git /tmp/fuse-backend-rs
git clone --depth 1 https://github.com/ejc3/fuser.git /tmp/fuser
chown -R ubuntu:ubuntu /tmp/fcvm /tmp/fuse-backend-rs /tmp/fuser
sudo -u ubuntu env HOME=/home/ubuntu bash -c \
  'source "$HOME/.cargo/env"; cd /tmp/fcvm; make build-host-tools'
cd /tmp/fcvm

# Build pasta/passt from the repo's pinned source so the AMI matches CI
# (scripts/build-passt.sh owns the pin and the local patch).
./scripts/build-passt.sh

# Use repo's config which has nested profile defined
mkdir -p /root/.config/fcvm
cp rootfs-config.toml /root/.config/fcvm/

# Build and install kernel using fcvm setup. It installs the kernel package with dpkg.
wait_for_dpkg_lock
./target/release/fcvm setup --kernel-profile nested --build-kernels --install-host-kernel

# The image must boot a kernel with the bridge teardown fix. The running kernel is still
# the base image's, so check the release GRUB will boot: regenerate grub.cfg (fcvm setup
# only warns when update-grub fails; here that fails the build) and read the default
# entry's kernel from it, which must be in /boot with its initrd.
update-grub
BOOT_KERNEL=$(./scripts/grub-boot-kernel.sh)
./scripts/check-bridge-teardown-kernel.sh "$BOOT_KERNEL"
# Report that release: the boot test of the candidate image must find exactly it running.
aws ec2 create-tags --resources $INSTANCE_ID --tags "Key=KernelVersion,Value=$BOOT_KERNEL" --region us-west-1
# The boot test runs the same check on the kernel that booted.
install -m 0755 scripts/check-bridge-teardown-kernel.sh /usr/local/sbin/check-bridge-teardown-kernel.sh

# Firecracker
FIRECRACKER_VERSION="v1.14.0"
curl --fail -L -o /tmp/firecracker.tgz \
  "https://github.com/firecracker-microvm/firecracker/releases/download/${FIRECRACKER_VERSION}/firecracker-${FIRECRACKER_VERSION}-${MACHINE}.tgz"
tar -xzf /tmp/firecracker.tgz -C /usr/local/bin --strip-components=1 \
  "release-${FIRECRACKER_VERSION}-${MACHINE}/firecracker-${FIRECRACKER_VERSION}-${MACHINE}" \
  "release-${FIRECRACKER_VERSION}-${MACHINE}/jailer-${FIRECRACKER_VERSION}-${MACHINE}"
mv "/usr/local/bin/firecracker-${FIRECRACKER_VERSION}-${MACHINE}" /usr/local/bin/firecracker
mv "/usr/local/bin/jailer-${FIRECRACKER_VERSION}-${MACHINE}" /usr/local/bin/jailer

# Podman rootless
echo "ubuntu:100000:65536" >> /etc/subuid
echo "ubuntu:100000:65536" >> /etc/subgid

# FUSE config
echo "user_allow_other" > /etc/fuse.conf

# GitHub Actions Runner
mkdir -p /opt/actions-runner
RUNNER_VERSION=$(curl -s https://api.github.com/repos/actions/runner/releases/latest | jq -r '.tag_name' | sed 's/v//')
curl --fail -o /tmp/actions-runner.tar.gz -L \
  "https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/actions-runner-linux-${RUNNER_ARCH}-${RUNNER_VERSION}.tar.gz"
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
apt_get clean
rm -f /etc/apt/apt.conf.d/99-fcvm-builder-lock-wait
rm -rf /var/lib/apt/lists/* /tmp/* /var/tmp/*

# Signal done
aws ec2 create-tags --resources $INSTANCE_ID --tags Key=BuildStatus,Value=complete --region us-west-1
USERDATA
}

# Wait for instance build to complete
# What the builder's user data printed last, from the instance's console. A builder that
# stops before it can tag itself has no other voice: on 2026-11-24 the cause of a leg that
# sat at "starting" was one apt error on its console, which this job could not read. This
# job's log is public and the lines are another machine's output, so: only cloud-init's
# own lines are printed, without its network and SSH key tables; ARNs, account numbers,
# host names, addresses and instance ids are masked; and every line carries a prefix, so
# none can start with `::` and be read by the Actions runner as a workflow command. A
# console this role cannot read is said so and changes nothing else.
print_builder_console() {
  local instance_id="$1" output
  if ! output=$(aws ec2 get-console-output --region "$REGION" --instance-id "$instance_id" \
    --latest --query 'Output' --output text 2> /dev/null); then
    echo "(the builder's console could not be read)"
    return 0
  fi
  echo "The builder's last cloud-init lines, from its console:"
  printf '%s\n' "$output" |
    grep -E 'cloud-init\[[0-9]+\]: ' |
    grep -v -E 'ci-info|ssh-|SHA256|BEGIN |END |ec2: |[Ff]ingerprint|authorized_keys' |
    sed -E -e 's/^.*cloud-init\[[0-9]+\]: //' \
      -e 's/arn:aws[a-z-]*:[^[:space:]"'"'"',)]*/<arn>/g' \
      -e 's/(ip|ec2)-[0-9]+(-[0-9]+){3}[a-z0-9.-]*/<host>/g' \
      -e 's/[0-9]{1,3}(\.[0-9]{1,3}){3}/<address>/g' \
      -e 's/(^|[^0-9A-Za-z:])([0-9A-Fa-f]{1,4}:){3,7}[0-9A-Fa-f]{1,4}/\1<address>/g' \
      -e 's/(^|[^0-9A-Za-z:])([0-9A-Fa-f]{1,4}:){1,6}(:[0-9A-Fa-f]{1,4}){1,6}/\1<address>/g' \
      -e 's/i-[0-9a-f]{8,17}/<instance>/g' \
      -e 's/(^|[^0-9])[0-9]{12}([^0-9]|$)/\1<account>\2/g' \
      -e 's/^/builder| /' |
    tail -n 30 || true
}

wait_for_build() {
  local instance_id="$1"
  local timeout="${2:-120}"  # 120 iterations * 30s = 60 minutes max
  # A builder tags itself "building" once it has installed the AWS CLI, about a minute
  # after launch. One still "starting" after this many 30 s polls stopped before it could
  # tag anything, or never ran its user data: 20 minutes, above the 760 s its two apt
  # calls before that tag can spend waiting for a lock (see apt_get in the user data).
  local starting_limit=40
  local status i

  echo "Waiting for build to complete..."
  for i in $(seq 1 "$timeout"); do
    status=$(aws ec2 describe-tags \
      --region "$REGION" \
      --filters "Name=resource-id,Values=$instance_id" "Name=key,Values=BuildStatus" \
      --query 'Tags[0].Value' --output text)
    echo "[$i/$timeout] Build status: $status (instance: $instance_id)"
    case "$status" in
      failed*)
        # The builder's handler puts the failing line and command after "failed:".
        echo "Build failed on the builder: $status"
        print_builder_console "$instance_id"
        return 1
        ;;
    esac
    if [ "$status" = "starting" ] && [ "$i" -ge "$starting_limit" ]; then
      echo "Build failed: the builder wrote no tag in $i polls. Its user data stopped before" \
        "the AWS CLI was installed, did not run, or could not tag the instance."
      print_builder_console "$instance_id"
      return 1
    fi
    if [ "$status" = "complete" ]; then
      return 0
    fi
    # CI cannot read arbitrary SSM command output. Its only progress channel is
    # the builder role's narrowly writable BuildStatus/KernelVersion tags.
    sleep 30
  done
  echo "Build timeout!"
  print_builder_console "$instance_id"
  return 1
}

# User data of the boot test: the candidate image's own first boot reports the kernel it
# is running through the instance's tags, the one channel the builder profile may write.
# boot-ok only when the check baked into the image passes on that running kernel.
create_boot_test_user_data() {
  cat << 'BOOTTEST'
#!/bin/bash
exec > >(tee /var/log/ami-boot-test.log) 2>&1
set -uo pipefail
# Tracing starts after the token's last use, as in the builder's user data.
TOKEN=$(curl -X PUT "http://169.254.169.254/latest/api/token" -H "X-aws-ec2-metadata-token-ttl-seconds: 600" -s)
INSTANCE_ID=$(curl -H "X-aws-ec2-metadata-token: $TOKEN" -s http://169.254.169.254/latest/meta-data/instance-id)
unset TOKEN
set -x
STATUS=boot-failed
if /usr/local/sbin/check-bridge-teardown-kernel.sh; then
  STATUS=boot-ok
fi
/usr/local/bin/aws ec2 create-tags --resources "$INSTANCE_ID" --region us-west-1 \
  --tags "Key=KernelVersion,Value=$(uname -r)" "Key=BuildStatus,Value=$STATUS"
BOOTTEST
}

# Launch one instance with the builder's constraints: spot first, on-demand when spot
# fails. Prints the instance id. All resources created by RunInstances need the scoped
# name tag. Keep one argument array so the on-demand fallback cannot silently regain
# privileges.
launch_instance() {
  local image="$1" instance_type="$2" user_data_file="$3" subnet="$4" security_group="$5"
  local launch_tags instance_id
  launch_tags='[{"ResourceType":"instance","Tags":[{"Key":"Name","Value":"ami-builder-temp"},{"Key":"BuildStatus","Value":"starting"}]},{"ResourceType":"volume","Tags":[{"Key":"Name","Value":"ami-builder-temp"}]},{"ResourceType":"network-interface","Tags":[{"Key":"Name","Value":"ami-builder-temp"}]}]'
  local -a launch_args=(
    --region "$REGION"
    --image-id "$image"
    --instance-type "$instance_type"
    --subnet-id "$subnet"
    --security-group-ids "$security_group"
    --iam-instance-profile Name=ami-builder-profile
    --associate-public-ip-address
    --metadata-options "HttpTokens=required,HttpPutResponseHopLimit=1"
    --block-device-mappings '[{"DeviceName":"/dev/sda1","Ebs":{"VolumeSize":40,"VolumeType":"gp3","Encrypted":true,"DeleteOnTermination":true}}]'
    --tag-specifications "$launch_tags"
    --user-data "file://$user_data_file"
    --query 'Instances[0].InstanceId'
    --output text
  )
  echo "Trying spot instance ($instance_type)..." >&2
  if ! instance_id=$(aws ec2 run-instances "${launch_args[@]}" \
    --instance-market-options '{"MarketType":"spot"}'); then
    echo "Spot failed, using on-demand..." >&2
    if ! instance_id=$(aws ec2 run-instances "${launch_args[@]}"); then
      echo "ERROR: could not launch $instance_type from $image" >&2
      return 1
    fi
  fi
  if [[ ! $instance_id =~ ^i-[0-9a-f]+$ ]]; then
    echo "ERROR: RunInstances did not return exactly one instance ID" >&2
    return 1
  fi
  printf '%s\n' "$instance_id"
}

# Wait until an image is available (custom loop: the default waiter times out on large
# disks). A failed image fails, and so does one still pending when the wait ends:
# publication depends on the image, so "not failed yet" is not success.
wait_for_image() {
  local ami_id="$1"
  local timeout="${2:-60}" # 60 iterations * 30s = 30 minutes max
  local state i

  echo "Waiting for AMI to be available..."
  for i in $(seq 1 "$timeout"); do
    state=$(aws ec2 describe-images --region "$REGION" --image-ids "$ami_id" \
      --query 'Images[0].State' --output text) || state=unknown
    echo "[$i/$timeout] AMI state: $state"
    if [ "$state" = "available" ]; then
      return 0
    fi
    if [ "$state" = "failed" ]; then
      echo "AMI creation failed!"
      return 1
    fi
    sleep 30
  done
  echo "AMI $ami_id is not available after $timeout checks"
  return 1
}

# Wait for the boot test instance to report (create_boot_test_user_data) and print the
# kernel it is running. An instance that reports boot-failed fails, and so does one that
# never reports before the wait ends: a kernel that does not boot, or boots without
# network, cannot say so.
wait_for_boot_test() {
  local instance_id="$1"
  local timeout="${2:-60}" # 60 iterations * 30s = 30 minutes max: bare metal boots slowly
  local status kernel i

  echo "Waiting for the boot test to report..." >&2
  for i in $(seq 1 "$timeout"); do
    status=$(aws ec2 describe-tags \
      --region "$REGION" \
      --filters "Name=resource-id,Values=$instance_id" "Name=key,Values=BuildStatus" \
      --query 'Tags[0].Value' --output text) || status=unknown
    echo "[$i/$timeout] Boot test status: $status (instance: $instance_id)" >&2
    if [ "$status" = "boot-ok" ] || [ "$status" = "boot-failed" ]; then
      kernel=$(aws ec2 describe-tags \
        --region "$REGION" \
        --filters "Name=resource-id,Values=$instance_id" "Name=key,Values=KernelVersion" \
        --query 'Tags[0].Value' --output text) || kernel=""
      if [ "$status" = "boot-failed" ] || [ -z "$kernel" ] || [ "$kernel" = "None" ]; then
        echo "Boot test failed: the image booted kernel '${kernel:-unknown}'" >&2
        return 1
      fi
      printf '%s\n' "$kernel"
      return 0
    fi
    sleep 30
  done
  echo "Boot test timeout: the instance never reported its kernel" >&2
  return 1
}

# A candidate that cannot be published stays registered (this role cannot deregister),
# tagged so that it is plainly not a runner image. No launcher selects either value.
reject_candidate() {
  local ami_id="$1"
  echo "Rejecting candidate $ami_id: it is tagged Purpose=github-runner-failed and not published"
  aws ec2 create-tags --region "$REGION" --resources "$ami_id" \
    --tags Key=Purpose,Value=github-runner-failed ||
    echo "WARNING: could not tag $ami_id failed; it keeps Purpose=github-runner-candidate" >&2
}

# A candidate left by an earlier run was never judged: the run was cancelled between
# create-image and the verdict, or its publish failed. Tag every candidate of this
# architecture failed before this run launches anything, so it can no longer be
# boot-tested or published. The workflow runs one leg per architecture at a time, so a candidate found
# here is never one a running leg is testing. A listing or a tag change that fails stops
# the leg: "no stale candidates" must be something this run saw.
retire_stale_candidates() {
  local stale id
  if ! stale=$(aws ec2 describe-images --region "$REGION" --owners self \
    --filters "Name=tag:Purpose,Values=github-runner-candidate" "Name=architecture,Values=$EC2_ARCH" \
    --query 'Images[].ImageId' --output text); then
    echo "ERROR: cannot list earlier candidate images" >&2
    return 1
  fi
  for id in $stale; do
    if [[ ! $id =~ ^ami-[0-9a-f]+$ ]]; then
      echo "ERROR: unexpected image id '$id' among earlier candidates" >&2
      return 1
    fi
    echo "Retiring stale candidate $id"
    if ! aws ec2 create-tags --region "$REGION" --resources "$id" \
      --tags Key=Purpose,Value=github-runner-failed; then
      echo "ERROR: cannot retire stale candidate $id" >&2
      return 1
    fi
  done
}

# Instances this run launched, terminated when it exits.
LAUNCHED=()

# Main
main() {
  local hash repo_root source_commit
  for tool in aws jq git; do
    command -v "$tool" >/dev/null 2>&1 || {
      echo "ERROR: required tool '$tool' is missing" >&2
      exit 1
    }
  done
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

  # A commit the builder cannot fetch is refused here, before this run changes anything
  # in AWS: the cache lookup above only reads.
  if ! verify_source_commit_fetchable \
    "$repo_root" "$source_commit" "$FCVM_SOURCE_REMOTE"; then
    exit 1
  fi

  echo "No cached AMI, building..."

  # The first write of a run that will build. A cache hit builds nothing and leaves an
  # earlier candidate to the pruner in ejc3/aws, which removes it a day after its creation.
  if ! retire_stale_candidates; then
    exit 1
  fi

  # Terraform's builder role permits only the isolated runner VPC. Discover its
  # current IDs instead of inheriting the administration/dev network or profile.
  local builder_subnet builder_security_group
  builder_subnet=$(aws ec2 describe-subnets --region "$REGION" \
    --filters "Name=tag:Name,Values=github-runner-subnet" \
    --query 'Subnets[].SubnetId' --output text)
  builder_security_group=$(aws ec2 describe-security-groups --region "$REGION" \
    --filters "Name=group-name,Values=github-runner-sg" \
    --query 'SecurityGroups[].GroupId' --output text)
  if [[ ! $builder_subnet =~ ^subnet-[0-9a-f]+$ ]] || \
    [[ ! $builder_security_group =~ ^sg-[0-9a-f]+$ ]]; then
    echo "ERROR: expected exactly one Terraform runner subnet and security group" >&2
    exit 1
  fi

  # Clean up any orphaned builder instances (from cancelled runs). Only this
  # architecture's: the other image's builder may be running beside this one.
  orphans=$(aws ec2 describe-instances \
    --region "$REGION" \
    --filters "Name=tag:Name,Values=ami-builder-temp" "Name=architecture,Values=$EC2_ARCH" \
    "Name=instance-state-name,Values=running,pending" \
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

  cleanup() {
    if [ "${#LAUNCHED[@]}" -gt 0 ]; then
      echo "Cleaning up instances ${LAUNCHED[*]}..."
      aws ec2 terminate-instances --region "$REGION" --instance-ids "${LAUNCHED[@]}" || true
    fi
  }
  trap cleanup EXIT

  if ! instance_id=$(launch_instance "$base_ami" "$BUILDER_TYPE" "$user_data_file" \
    "$builder_subnet" "$builder_security_group"); then
    exit 1
  fi
  LAUNCHED+=("$instance_id")
  echo "Launched instance: $instance_id"

  # Wait for build
  if ! wait_for_build "$instance_id"; then
    echo "Build failed!"
    exit 1
  fi

  # No stop before create-image: the builder is launched as a one-time spot
  # instance when spot capacity exists, and AWS rejects StopInstances on
  # one-time spot requests (UnsupportedOperation). create-image's default
  # reboot is permitted on spot and gives the same filesystem consistency a
  # stopped instance would.

  # The release the builder resolved from the image's grub.cfg, from its tags. Nothing is
  # created unless the builder reported one that has the fix.
  kernel_version=$(aws ec2 describe-tags \
    --region "$REGION" \
    --filters "Name=resource-id,Values=$instance_id" "Name=key,Values=KernelVersion" \
    --query 'Tags[0].Value' --output text 2>/dev/null || echo "unknown")
  if ! "$SCRIPT_DIR/check-bridge-teardown-kernel.sh" "$kernel_version"; then
    echo "ERROR: the builder did not report a kernel with the fix (KernelVersion=$kernel_version)" >&2
    exit 1
  fi

  # Create the image as a CANDIDATE. The runner launcher takes the newest available
  # image tagged Purpose=github-runner of each architecture, so an image created with
  # that tag replaces the working one for the whole pool before anything has booted
  # it. The candidate value is one no launcher selects.
  timestamp=$(date +%Y%m%d-%H%M)
  ami_name="fcvm-runner-${kernel_version}-${AMI_ARCH}-${timestamp}"
  local image_tags
  image_tags=$(jq -cn --arg name "$ami_name" --arg kernel "$kernel_version" --arg hash "$hash" \
    '[{ResourceType:"image",Tags:[{Key:"Name",Value:$name},{Key:"Kernel",Value:$kernel},{Key:"BuildHash",Value:$hash},{Key:"Purpose",Value:"github-runner-candidate"}]},{ResourceType:"snapshot",Tags:[{Key:"Purpose",Value:"github-runner"}]}]')

  ami_id=$(aws ec2 create-image \
    --region "$REGION" \
    --instance-id "$instance_id" \
    --name "$ami_name" \
    --description "fcvm CI runner ($AMI_ARCH) with kernel $kernel_version" \
    --tag-specifications "$image_tags" \
    --query 'ImageId' --output text)
  if [[ ! $ami_id =~ ^ami-[0-9a-f]+$ ]]; then
    echo "ERROR: CreateImage did not return exactly one image ID" >&2
    exit 1
  fi
  echo "Created candidate AMI: $ami_id ($ami_name)"

  if ! wait_for_image "$ami_id"; then
    reject_candidate "$ami_id"
    exit 1
  fi

  # Boot test: one instance from the candidate, on bare metal as the runners are. It
  # must report that the kernel it is RUNNING passes the bridge teardown check, and that
  # kernel must be the one the builder resolved, which passed the same check above.
  local boot_user_data_file test_instance boot_kernel
  boot_user_data_file=$(mktemp)
  create_boot_test_user_data > "$boot_user_data_file"
  if ! test_instance=$(launch_instance "$ami_id" "$BOOT_TEST_TYPE" "$boot_user_data_file" \
    "$builder_subnet" "$builder_security_group"); then
    reject_candidate "$ami_id"
    exit 1
  fi
  LAUNCHED+=("$test_instance")
  echo "Boot test instance: $test_instance ($BOOT_TEST_TYPE)"
  if ! boot_kernel=$(wait_for_boot_test "$test_instance"); then
    reject_candidate "$ami_id"
    exit 1
  fi
  if [ "$boot_kernel" != "$kernel_version" ]; then
    echo "ERROR: the image booted $boot_kernel, not the $kernel_version the builder installed" >&2
    reject_candidate "$ami_id"
    exit 1
  fi
  echo "Boot test passed: $ami_id booted $boot_kernel on $BOOT_TEST_TYPE"

  # Publish: from here the launcher selects the image.
  aws ec2 create-tags --region "$REGION" --resources "$ami_id" \
    --tags Key=Purpose,Value=github-runner

  echo "SUCCESS: $ami_id"
  {
    echo "ami_id=$ami_id"
    echo "kernel_version=$kernel_version"
    echo "boot_kernel=$boot_kernel"
    echo "boot_test_type=$BOOT_TEST_TYPE"
    echo "cached=false"
  } >> "${GITHUB_OUTPUT:-/dev/null}"
}

main "$@"
