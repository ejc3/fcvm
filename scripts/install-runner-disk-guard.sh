#!/bin/bash
# Install the runner disk guard and its privileged target-pruning helper from
# one source checkout. DESTDIR is optional and exists so the exact deployment
# operation can be exercised without writing to the host filesystem.
set -euo pipefail

if (($# < 1 || $# > 2)); then
  printf 'usage: %s SOURCE_ROOT [DESTDIR]\n' "$0" >&2
  exit 2
fi

source_root="${1%/}"
destination="${2:-}"
destination="${destination%/}"
bin_dir="$destination/usr/local/bin"
unit_dir="$destination/etc/systemd/system"

install -d -m 755 "$bin_dir" "$unit_dir"
install -m 755 "$source_root/scripts/runner-disk-preflight.sh" \
  "$bin_dir/runner-disk-preflight.sh"
install -m 755 "$source_root/scripts/prune-cargo-target.sh" \
  "$bin_dir/prune-cargo-target.sh"
install -m 644 "$source_root/scripts/runner-disk-guard.service" \
  "$unit_dir/runner-disk-guard.service"
install -m 644 "$source_root/scripts/runner-disk-guard.timer" \
  "$unit_dir/runner-disk-guard.timer"
