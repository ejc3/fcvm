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
info() { echo -e "${GREEN}==>${NC} $*" >&2; }
warn() { echo -e "${YELLOW}WARNING:${NC} $*"; }

# Resolve the config architecture, with an override for validating another
# architecture's patch set from the current host.
get_config_arch() {
    local machine_arch="${KERNEL_PATCH_ARCH:-$(uname -m)}"

    case "$machine_arch" in
        aarch64|arm64) printf '%s\n' "arm64" ;;
        x86_64|amd64) printf '%s\n' "amd64" ;;
        *) error "Unsupported architecture: $machine_arch" ;;
    esac
}

# Get kernel version from config
get_kernel_version() {
    local profile="$1"
    local config_file="$REPO_ROOT/rootfs-config.toml"
    local config_arch version

    if [[ ! -f "$config_file" ]]; then
        error "Config file not found: $config_file"
    fi

    config_arch=$(get_config_arch) || return 1

    # Match the exact architecture-specific TOML section. Treating the section
    # name as a grep regex makes its brackets a character class and silently
    # returns an empty version.
    version=$(awk -v section="[kernel_profiles.${profile}.${config_arch}]" '
        $0 == section { in_section = 1; next }
        in_section && /^\[/ { exit }
        in_section && /^[[:space:]]*kernel_version[[:space:]]*=/ {
            value = $0
            sub(/^[^=]*=[[:space:]]*"/, "", value)
            sub(/"[[:space:]]*$/, "", value)
            print value
            exit
        }
    ' "$config_file")

    [[ -n "$version" ]] || error \
        "Could not find kernel_version in [kernel_profiles.${profile}.${config_arch}]"
    printf '%s\n' "$version"
}

# Get patches directory for profile
get_patches_dir() {
    local profile="$1"
    local config_file="$REPO_ROOT/rootfs-config.toml"
    local config_arch declared

    if [[ ! -f "$config_file" ]]; then
        error "Config file not found: $config_file"
    fi

    config_arch=$(get_config_arch) || return 1

    # A profile may name its own patches_dir, and the Rust build reads it
    # (src/setup/kernel.rs). This helper has to agree, or it edits and validates
    # a different set of patches than the one that actually gets built.
    if declared=$(awk -v section="[kernel_profiles.${profile}.${config_arch}]" '
        $0 == section { in_section = 1; next }
        in_section && /^\[/ { exit 1 }
        in_section && /^[[:space:]]*patches_dir[[:space:]]*=/ {
            value = $0
            sub(/^[^=]*=[[:space:]]*"/, "", value)
            sub(/"[[:space:]]*$/, "", value)
            print value
            found = 1
            exit
        }
        END { if (!found) exit 1 }
    ' "$config_file"); then
        # An empty string means the profile deliberately applies no patches.
        [[ -z "$declared" ]] && return
        printf '%s\n' "$REPO_ROOT/$declared"
        return
    fi

    # Preserve the legacy default for profiles without an explicit directory.
    if [[ "$config_arch" == "arm64" && -d "$REPO_ROOT/kernel/patches-arm64" ]]; then
        printf '%s\n' "$REPO_ROOT/kernel/patches-arm64"
    else
        printf '%s\n' "$REPO_ROOT/kernel/patches"
    fi
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
        curl -fL -o "$tarball" "$url" || error "Failed to download kernel"
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

    local version patches_dir
    version=$(get_kernel_version "$profile") || error "Could not resolve kernel version for profile '$profile'"
    patches_dir=$(get_patches_dir "$profile") || error "Could not resolve patches_dir for profile '$profile'"
    local workdir="/tmp/kernel-patch-$$"

    # An empty patches_dir means the profile deliberately applies no patches.
    [[ -z "$patches_dir" ]] && error "Profile '$profile' has patches disabled (patches_dir is empty); cannot create a patch"

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

    local version patches_dir
    version=$(get_kernel_version "$profile") || error "Could not resolve kernel version for profile '$profile'"
    patches_dir=$(get_patches_dir "$profile") || error "Could not resolve patches_dir for profile '$profile'"
    local kernel_dir="$workdir/linux-${version}"

    # An empty patches_dir means the profile deliberately applies no patches.
    [[ -z "$patches_dir" ]] && error "Profile '$profile' has patches disabled (patches_dir is empty); cannot finish a patch"

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

    local version patches_dir
    version=$(get_kernel_version "$profile") || error "Could not resolve kernel version for profile '$profile'"
    patches_dir=$(get_patches_dir "$profile") || error "Could not resolve patches_dir for profile '$profile'"
    local workdir="/tmp/kernel-patch-$$"

    # An empty patches_dir means the profile deliberately applies no patches.
    [[ -z "$patches_dir" ]] && error "Profile '$profile' has patches disabled (patches_dir is empty); cannot edit a patch"

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

    local version patches_dir
    version=$(get_kernel_version "$profile") || error "Could not resolve kernel version for profile '$profile'"
    patches_dir=$(get_patches_dir "$profile") || error "Could not resolve patches_dir for profile '$profile'"
    local workdir="/tmp/kernel-validate-$$"

    # An empty patches_dir means the profile deliberately applies no patches.
    if [[ -z "$patches_dir" ]]; then
        info "Profile '$profile' has patches disabled (patches_dir is empty); nothing to validate"
        return
    fi

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
