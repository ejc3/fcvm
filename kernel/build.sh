#!/bin/bash
# Build a custom Linux kernel with FUSE and KVM support for fcvm nested virtualization
#
# Required env vars:
#   KERNEL_PATH - output path (caller computes SHA-based filename)
#
# Optional env vars:
#   KERNEL_VERSION - kernel version (default: 6.12.10)
#   BUILD_DIR - build directory (default: /tmp/kernel-build)
#   NPROC - parallel jobs (default: nproc)

set -euo pipefail

# Configuration
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Compute SHA from build inputs if KERNEL_PATH not provided
compute_sha() {
    # SHA is based on: build.sh + nested.conf + all patches
    (
        cat "$SCRIPT_DIR/build.sh"
        cat "$SCRIPT_DIR/nested.conf"
        cat "$SCRIPT_DIR/patches/"*.patch 2>/dev/null || true
    ) | sha256sum | cut -c1-12
}

# Set KERNEL_PATH if not provided
if [[ -z "${KERNEL_PATH:-}" ]]; then
    KERNEL_VERSION="${KERNEL_VERSION:-6.18.3}"
    BUILD_SHA=$(compute_sha)
    KERNEL_PATH="/mnt/fcvm-btrfs/kernels/vmlinux-${KERNEL_VERSION}-${BUILD_SHA}.bin"
    echo "Computed KERNEL_PATH: $KERNEL_PATH"
fi

# Configuration (may already be set above)
KERNEL_VERSION="${KERNEL_VERSION:-6.18.3}"
KERNEL_MAJOR="${KERNEL_VERSION%%.*}"
BUILD_DIR="${BUILD_DIR:-/tmp/kernel-build}"
NPROC="${NPROC:-$(nproc)}"

# Architecture detection
ARCH=$(uname -m)
case "$ARCH" in
    aarch64) KERNEL_ARCH=arm64; KERNEL_IMAGE=Image ;;
    x86_64)  KERNEL_ARCH=x86_64; KERNEL_IMAGE=bzImage ;;
    *)       echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac

echo "=== fcvm Nested Kernel Build ==="
echo "Kernel version: $KERNEL_VERSION"
echo "Architecture: $KERNEL_ARCH"
echo "Output: $KERNEL_PATH"
echo ""

# Check if already built
if [[ -f "$KERNEL_PATH" ]]; then
    echo "Kernel already exists: $KERNEL_PATH"
    echo "Skipping build."
    exit 0
fi

# Create directories
mkdir -p "$(dirname "$KERNEL_PATH")" "$BUILD_DIR"
cd "$BUILD_DIR"

# Download kernel source if needed
KERNEL_TARBALL="linux-${KERNEL_VERSION}.tar.xz"
KERNEL_URL="https://cdn.kernel.org/pub/linux/kernel/v${KERNEL_MAJOR}.x/${KERNEL_TARBALL}"

if [[ ! -f "$KERNEL_TARBALL" ]]; then
    echo "Downloading kernel source..."
    curl -fSL "$KERNEL_URL" -o "$KERNEL_TARBALL"
fi

if [[ ! -d "linux-${KERNEL_VERSION}" ]]; then
    echo "Extracting kernel source..."
    tar xf "$KERNEL_TARBALL"
fi

cd "linux-${KERNEL_VERSION}"

# Apply patches from patches directory
# We manually apply our FUSE remap patch since git-format patches require
# special handling in a non-git directory
PATCHES_DIR="$SCRIPT_DIR/patches"
echo "Applying FUSE patches..."

# Check if FUSE_REMAP_FILE_RANGE is already defined
if grep -q "FUSE_REMAP_FILE_RANGE" include/uapi/linux/fuse.h; then
    echo "  FUSE remap_file_range support already applied"
else
    echo "  Adding FUSE_REMAP_FILE_RANGE opcode and struct..."
    # Add opcode after FUSE_STATX
    sed -i '/FUSE_STATX.*= 52,$/a\	FUSE_REMAP_FILE_RANGE	= 54,' include/uapi/linux/fuse.h

    # Add struct before #endif
    cat >> include/uapi/linux/fuse.h.tmp << 'STRUCT'

/**
 * struct fuse_remap_file_range_in - FUSE_REMAP_FILE_RANGE request
 */
struct fuse_remap_file_range_in {
	uint64_t	fh_in;
	uint64_t	off_in;
	uint64_t	nodeid_out;
	uint64_t	fh_out;
	uint64_t	off_out;
	uint64_t	len;
	uint32_t	remap_flags;
	uint32_t	padding;
};

STRUCT
    sed -i '/#endif.*_LINUX_FUSE_H/e cat include/uapi/linux/fuse.h.tmp' include/uapi/linux/fuse.h
    rm -f include/uapi/linux/fuse.h.tmp

    # Add field to fuse_i.h
    sed -i '/unsigned no_copy_file_range:1;/a\
\
	/** Does the filesystem support remap_file_range (FICLONE)? */\
	unsigned no_remap_file_range:1;' fs/fuse/fuse_i.h

    # Add function to file.c (before fuse_file_operations struct)
    LINE=$(grep -n "^static const struct file_operations fuse_file_operations" fs/fuse/file.c | cut -d: -f1)
    head -n $((LINE-1)) fs/fuse/file.c > fs/fuse/file.c.tmp
    cat "$PATCHES_DIR/fuse_remap_function.c" >> fs/fuse/file.c.tmp 2>/dev/null || cat >> fs/fuse/file.c.tmp << 'FUNC'

static loff_t fuse_remap_file_range(struct file *file_in, loff_t pos_in,
				    struct file *file_out, loff_t pos_out,
				    loff_t len, unsigned int remap_flags)
{
	struct fuse_file *ff_in = file_in->private_data;
	struct fuse_file *ff_out = file_out->private_data;
	struct inode *inode_in = file_inode(file_in);
	struct inode *inode_out = file_inode(file_out);
	struct fuse_inode *fi_out = get_fuse_inode(inode_out);
	struct fuse_mount *fm = ff_in->fm;
	struct fuse_conn *fc = fm->fc;
	FUSE_ARGS(args);
	struct fuse_remap_file_range_in inarg = {
		.fh_in = ff_in->fh,
		.off_in = pos_in,
		.nodeid_out = ff_out->nodeid,
		.fh_out = ff_out->fh,
		.off_out = pos_out,
		.len = len,
		.remap_flags = remap_flags,
	};
	struct fuse_write_out outarg;
	loff_t err;
	loff_t end_in, end_out;
	bool is_unstable;

	if (fc->no_remap_file_range)
		return -EOPNOTSUPP;

	if (file_inode(file_in)->i_sb != file_inode(file_out)->i_sb)
		return -EXDEV;

	if (len == 0) {
		end_in = LLONG_MAX;
		end_out = LLONG_MAX;
	} else {
		end_in = pos_in + len - 1;
		end_out = pos_out + len - 1;
	}

	is_unstable = (!fc->writeback_cache) &&
		      (len == 0 || (pos_out + len) > inode_out->i_size);

	inode_lock(inode_in);
	err = fuse_writeback_range(inode_in, pos_in, end_in);
	inode_unlock(inode_in);
	if (err)
		return err;

	inode_lock(inode_out);

	err = file_modified(file_out);
	if (err)
		goto out;

	err = fuse_writeback_range(inode_out, pos_out, end_out);
	if (err)
		goto out;

	if (is_unstable)
		set_bit(FUSE_I_SIZE_UNSTABLE, &fi_out->state);

	args.opcode = FUSE_REMAP_FILE_RANGE;
	args.nodeid = ff_in->nodeid;
	args.in_numargs = 1;
	args.in_args[0].size = sizeof(inarg);
	args.in_args[0].value = &inarg;
	args.out_numargs = 1;
	args.out_args[0].size = sizeof(outarg);
	args.out_args[0].value = &outarg;

	err = fuse_simple_request(fm, &args);
	if (err == -ENOSYS) {
		fc->no_remap_file_range = 1;
		err = -EOPNOTSUPP;
		goto out;
	}
	if (err)
		goto out;

	truncate_inode_pages_range(inode_out->i_mapping,
				   ALIGN_DOWN(pos_out, PAGE_SIZE),
				   ALIGN(pos_out + outarg.size, PAGE_SIZE) - 1);

	file_update_time(file_out);
	fuse_write_update_attr(inode_out, pos_out + outarg.size, outarg.size);

	err = outarg.size;
out:
	if (is_unstable)
		clear_bit(FUSE_I_SIZE_UNSTABLE, &fi_out->state);

	inode_unlock(inode_out);
	file_accessed(file_in);
	fuse_flush_time_update(inode_out);

	return err;
}
FUNC
    tail -n +$LINE fs/fuse/file.c >> fs/fuse/file.c.tmp
    mv fs/fuse/file.c.tmp fs/fuse/file.c

    # Add to file_operations struct
    sed -i '/\.copy_file_range = fuse_copy_file_range,/a\
	.remap_file_range = fuse_remap_file_range,' fs/fuse/file.c

    echo "  FUSE remap_file_range support applied successfully"
fi

# Apply MMFR4 override patch for NV2 recursive nesting
MMFR4_PATCH="$PATCHES_DIR/mmfr4-override.patch"
if [[ -f "$MMFR4_PATCH" ]]; then
    if grep -q "id_aa64mmfr4_override" arch/arm64/kernel/cpufeature.c 2>/dev/null; then
        echo "  MMFR4 override support already applied"
    else
        echo "  Applying MMFR4 override patch for NV2 recursive nesting..."
        patch -p1 < "$MMFR4_PATCH"
        echo "  MMFR4 override patch applied successfully"
    fi
fi

# Apply any additional patches (excluding specially-handled ones)
echo "Checking for additional patches..."
for patch_file in "$PATCHES_DIR"/*.patch; do
    [[ ! -f "$patch_file" ]] && continue
    patch_name=$(basename "$patch_file")

    # Skip patches handled above
    case "$patch_name" in
        0001-fuse-add-remap_file_range-support.patch) continue ;;
        mmfr4-override.patch) continue ;;
    esac

    echo "  Applying $patch_name..."
    if patch -p1 --forward --dry-run < "$patch_file" >/dev/null 2>&1; then
        patch -p1 --forward < "$patch_file"
        echo "    Applied successfully"
    else
        # Check if already applied
        if patch -p1 --reverse --dry-run < "$patch_file" >/dev/null 2>&1; then
            echo "    Already applied (skipping)"
        else
            echo "    WARNING: Patch does not apply cleanly"
            patch -p1 --forward --dry-run < "$patch_file" || true
        fi
    fi
done

# Download Firecracker base config
FC_CONFIG_URL="https://raw.githubusercontent.com/firecracker-microvm/firecracker/main/resources/guest_configs/microvm-kernel-ci-${ARCH}-6.1.config"
echo "Downloading Firecracker base config..."
curl -fSL "$FC_CONFIG_URL" -o .config

# Apply options from nested.conf
echo "Applying options from nested.conf..."
KERNEL_CONF="$SCRIPT_DIR/nested.conf"
if [[ -f "$KERNEL_CONF" ]]; then
    # Parse each CONFIG_*=y line and enable it
    while IFS= read -r line; do
        # Skip comments and empty lines
        [[ "$line" =~ ^[[:space:]]*# ]] && continue
        [[ -z "${line// }" ]] && continue
        # Extract option name (everything before =)
        if [[ "$line" =~ ^(CONFIG_[A-Z0-9_]+)=y ]]; then
            opt="${BASH_REMATCH[1]}"
            echo "  Enabling $opt"
            ./scripts/config --enable "$opt"
        fi
    done < "$KERNEL_CONF"
else
    echo "  WARNING: $KERNEL_CONF not found"
fi

# Also enable BTRFS (always needed for fcvm)
./scripts/config --enable CONFIG_BTRFS_FS

# Update config with defaults for new options
make ARCH="$KERNEL_ARCH" olddefconfig

# Show enabled options
echo ""
echo "Verifying configuration:"
grep -E "^CONFIG_(FUSE_FS|KVM|VIRTUALIZATION|BTRFS_FS|TUN|VETH)=" .config || true
echo ""

# Build kernel
echo "Building kernel with $NPROC parallel jobs..."
make ARCH="$KERNEL_ARCH" -j"$NPROC" "$KERNEL_IMAGE"

# Copy output
echo "Copying kernel to $KERNEL_PATH..."
case "$KERNEL_ARCH" in
    arm64)  cp "arch/arm64/boot/Image" "$KERNEL_PATH" ;;
    x86_64) cp "arch/x86/boot/bzImage" "$KERNEL_PATH" ;;
esac

echo ""
echo "=== Build Complete ==="
echo "Kernel: $KERNEL_PATH"
echo "Size: $(du -h "$KERNEL_PATH" | cut -f1)"
