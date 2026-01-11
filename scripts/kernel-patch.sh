#!/bin/bash
#
# Helper script to create properly-formatted kernel patches
#
# Usage:
#   ./scripts/kernel-patch.sh create <profile> <patch-name> <file1> [file2...]
#   ./scripts/kernel-patch.sh edit <profile> <patch-number>
#   ./scripts/kernel-patch.sh validate <profile>
#
# Examples:
#   # Create a new patch for fs/fuse/dir.c
#   ./scripts/kernel-patch.sh create nested 0004-my-fix fs/fuse/dir.c
#
#   # Edit an existing patch
#   ./scripts/kernel-patch.sh edit nested 0002
#
#   # Validate all patches for a profile apply cleanly
#   ./scripts/kernel-patch.sh validate nested
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

error() { echo -e "${RED}ERROR:${NC} $*" >&2; exit 1; }
info() { echo -e "${GREEN}==>${NC} $*"; }
warn() { echo -e "${YELLOW}WARNING:${NC} $*"; }

# Get kernel version from config
get_kernel_version() {
    local profile="$1"
    local config_file="$REPO_ROOT/rootfs-config.toml"

    if [[ ! -f "$config_file" ]]; then
        error "Config file not found: $config_file"
    fi

    # Extract kernel version for the profile
    # This is a simple grep - for complex configs, use a proper TOML parser
    grep -A20 "\[kernel_profiles\.$profile\]" "$config_file" | \
        grep -m1 'kernel_version' | \
        sed 's/.*=.*"\([^"]*\)".*/\1/' || \
        error "Could not find kernel_version for profile '$profile'"
}

# Get patches directory for profile
get_patches_dir() {
    local profile="$1"

    # Check for arch-specific patches first
    local arch=$(uname -m)
    if [[ "$arch" == "aarch64" ]]; then
        local patches_dir="$REPO_ROOT/kernel/patches-arm64"
        if [[ -d "$patches_dir" ]]; then
            echo "$patches_dir"
            return
        fi
    fi

    # Fall back to generic patches
    echo "$REPO_ROOT/kernel/patches"
}

# Download and extract kernel source
setup_kernel_source() {
    local version="$1"
    local workdir="$2"

    local major_version="${version%%.*}"
    local tarball="linux-${version}.tar.xz"
    local url="https://cdn.kernel.org/pub/linux/kernel/v${major_version}.x/${tarball}"

    info "Setting up kernel $version in $workdir"

    mkdir -p "$workdir"
    cd "$workdir"

    if [[ ! -f "$tarball" ]]; then
        info "Downloading kernel source..."
        curl -L -o "$tarball" "$url" || error "Failed to download kernel"
    fi

    if [[ ! -d "linux-${version}" ]]; then
        info "Extracting kernel source..."
        tar xf "$tarball" || error "Failed to extract kernel"
    fi

    cd "linux-${version}"

    # Initialize git repo for proper patch generation
    if [[ ! -d ".git" ]]; then
        info "Initializing git repo..."
        git init -q
        git add -A
        git commit -q -m "Initial kernel $version"
    fi

    echo "$workdir/linux-${version}"
}

# Apply existing patches up to (but not including) a specific one
apply_patches_until() {
    local kernel_dir="$1"
    local patches_dir="$2"
    local stop_at="$3"  # e.g., "0002" or empty to apply all

    cd "$kernel_dir"

    # Reset to initial state
    git checkout -q .
    git clean -fdq

    for patch in "$patches_dir"/*.patch; do
        [[ -f "$patch" ]] || continue

        local patch_name=$(basename "$patch")
        local patch_num="${patch_name:0:4}"

        # Stop if we've reached the target patch
        if [[ -n "$stop_at" && "$patch_num" == "$stop_at" ]]; then
            break
        fi

        info "Applying $patch_name..."
        if ! git apply --check "$patch" 2>/dev/null; then
            # Try with -3 for 3-way merge
            if ! patch -p1 --dry-run < "$patch" >/dev/null 2>&1; then
                warn "Patch $patch_name may not apply cleanly"
            fi
        fi
        patch -p1 < "$patch" || error "Failed to apply $patch_name"
    done

    git add -A
    git commit -q -m "Applied patches" --allow-empty
}

# Generate a patch from current changes
generate_patch() {
    local kernel_dir="$1"
    local output_file="$2"
    local subject="$3"
    local description="$4"

    cd "$kernel_dir"

    # Stage all changes
    git add -A

    # Check if there are changes
    if git diff --cached --quiet; then
        error "No changes to create patch from"
    fi

    # Create commit
    git commit -q -m "$subject" -m "$description"

    # Generate patch
    git format-patch -1 --stdout > "$output_file"

    # Add fcvm signature
    sed -i "s/^From: .*/From: ejc3 <ejc3@users.noreply.github.com>/" "$output_file"

    info "Generated patch: $output_file"

    # Validate it
    git reset -q HEAD~1
    git checkout -q .

    if patch -p1 --dry-run < "$output_file" >/dev/null 2>&1; then
        info "Patch validates OK"
    else
        warn "Patch may have issues - please verify manually"
    fi
}

cmd_create() {
    local profile="${1:-}"
    local patch_name="${2:-}"
    shift 2 || true
    local files=("$@")

    [[ -z "$profile" ]] && error "Usage: $0 create <profile> <patch-name> <file1> [file2...]"
    [[ -z "$patch_name" ]] && error "Usage: $0 create <profile> <patch-name> <file1> [file2...]"
    [[ ${#files[@]} -eq 0 ]] && error "Usage: $0 create <profile> <patch-name> <file1> [file2...]"

    local version=$(get_kernel_version "$profile")
    local patches_dir=$(get_patches_dir "$profile")
    local workdir="/tmp/kernel-patch-$$"

    info "Creating patch for kernel $version (profile: $profile)"

    # Setup kernel source
    local kernel_dir=$(setup_kernel_source "$version" "$workdir")

    # Apply existing patches
    apply_patches_until "$kernel_dir" "$patches_dir" ""

    # Mark current state
    cd "$kernel_dir"
    git add -A
    git commit -q -m "Pre-edit state" --allow-empty

    echo ""
    echo "=========================================="
    echo "Kernel source ready at: $kernel_dir"
    echo ""
    echo "Files to edit:"
    for f in "${files[@]}"; do
        echo "  $kernel_dir/$f"
    done
    echo ""
    echo "When done editing, run:"
    echo "  $0 finish $profile $patch_name $workdir"
    echo ""
    echo "Or to abort:"
    echo "  rm -rf $workdir"
    echo "=========================================="
}

cmd_finish() {
    local profile="${1:-}"
    local patch_name="${2:-}"
    local workdir="${3:-}"

    [[ -z "$profile" ]] && error "Usage: $0 finish <profile> <patch-name> <workdir>"
    [[ -z "$patch_name" ]] && error "Usage: $0 finish <profile> <patch-name> <workdir>"
    [[ -z "$workdir" ]] && error "Usage: $0 finish <profile> <patch-name> <workdir>"

    local version=$(get_kernel_version "$profile")
    local patches_dir=$(get_patches_dir "$profile")
    local kernel_dir="$workdir/linux-${version}"

    [[ -d "$kernel_dir" ]] || error "Kernel dir not found: $kernel_dir"

    # Ensure patch name has .patch extension
    [[ "$patch_name" == *.patch ]] || patch_name="${patch_name}.patch"

    local output_file="$patches_dir/$patch_name"

    echo ""
    read -p "Enter patch subject (one line): " subject
    echo "Enter patch description (end with Ctrl-D):"
    description=$(cat)

    generate_patch "$kernel_dir" "$output_file" "$subject" "$description"

    echo ""
    info "Patch created: $output_file"
    echo ""
    echo "To clean up: rm -rf $workdir"
}

cmd_edit() {
    local profile="${1:-}"
    local patch_num="${2:-}"

    [[ -z "$profile" ]] && error "Usage: $0 edit <profile> <patch-number>"
    [[ -z "$patch_num" ]] && error "Usage: $0 edit <profile> <patch-number>"

    local version=$(get_kernel_version "$profile")
    local patches_dir=$(get_patches_dir "$profile")
    local workdir="/tmp/kernel-patch-$$"

    # Find the patch file
    local patch_file=$(ls "$patches_dir"/${patch_num}*.patch 2>/dev/null | head -1)
    [[ -f "$patch_file" ]] || error "No patch found matching: ${patch_num}*.patch"

    local patch_name=$(basename "$patch_file")

    info "Editing patch $patch_name for kernel $version"

    # Setup kernel source
    local kernel_dir=$(setup_kernel_source "$version" "$workdir")

    # Apply patches up to (but not including) this one
    apply_patches_until "$kernel_dir" "$patches_dir" "$patch_num"

    # Apply the target patch
    cd "$kernel_dir"
    info "Applying $patch_name..."
    patch -p1 < "$patch_file" || warn "Patch applied with issues"

    git add -A
    git commit -q -m "Applied $patch_name" --allow-empty

    echo ""
    echo "=========================================="
    echo "Kernel source ready at: $kernel_dir"
    echo "Current patch applied: $patch_name"
    echo ""
    echo "Make your edits, then run:"
    echo "  $0 finish $profile $patch_name $workdir"
    echo ""
    echo "Or to abort:"
    echo "  rm -rf $workdir"
    echo "=========================================="
}

cmd_validate() {
    local profile="${1:-}"

    [[ -z "$profile" ]] && error "Usage: $0 validate <profile>"

    local version=$(get_kernel_version "$profile")
    local patches_dir=$(get_patches_dir "$profile")
    local workdir="/tmp/kernel-validate-$$"

    info "Validating patches for kernel $version (profile: $profile)"

    # Setup kernel source
    local kernel_dir=$(setup_kernel_source "$version" "$workdir")

    cd "$kernel_dir"

    local failed=0
    for patch in "$patches_dir"/*.patch; do
        [[ -f "$patch" ]] || continue

        local patch_name=$(basename "$patch")

        if patch -p1 --dry-run < "$patch" >/dev/null 2>&1; then
            echo -e "  ${GREEN}✓${NC} $patch_name"
            patch -p1 < "$patch" >/dev/null
        else
            echo -e "  ${RED}✗${NC} $patch_name"
            failed=1
        fi
    done

    rm -rf "$workdir"

    if [[ $failed -eq 0 ]]; then
        info "All patches valid!"
    else
        error "Some patches failed validation"
    fi
}

# Main
case "${1:-}" in
    create)
        shift
        cmd_create "$@"
        ;;
    finish)
        shift
        cmd_finish "$@"
        ;;
    edit)
        shift
        cmd_edit "$@"
        ;;
    validate)
        shift
        cmd_validate "$@"
        ;;
    *)
        echo "Usage: $0 <command> [args...]"
        echo ""
        echo "Commands:"
        echo "  create <profile> <patch-name> <file1> [file2...]"
        echo "      Start creating a new patch"
        echo ""
        echo "  edit <profile> <patch-number>"
        echo "      Edit an existing patch (e.g., edit nested 0002)"
        echo ""
        echo "  finish <profile> <patch-name> <workdir>"
        echo "      Finish editing and generate the patch file"
        echo ""
        echo "  validate <profile>"
        echo "      Validate all patches apply cleanly"
        echo ""
        echo "Examples:"
        echo "  $0 create nested 0004-my-fix fs/fuse/dir.c"
        echo "  $0 edit nested 0002"
        echo "  $0 validate nested"
        exit 1
        ;;
esac
