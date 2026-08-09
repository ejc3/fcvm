#!/usr/bin/env bash
# Run a host test command with the exact current worktree's embedded fcvm config.
#
# ~/.config/fcvm is shared by every checkout. A concurrent `make build` in a
# different worktree can replace it with a valid config for that other branch
# between this worktree's setup and VM launch. A unique directory per test run
# removes the shared writer entirely; keeping it below the already per-worktree
# cargo target also keeps the temporary state scoped to this checkout.
set -euo pipefail

for tool in mktemp; do
	command -v "$tool" >/dev/null 2>&1 || {
		echo "BLOCKED: '$tool' is required for isolated test config" >&2
		exit 2
	}
done

if [ ! -x ./target/release/fcvm ]; then
	echo "BLOCKED: ./target/release/fcvm is missing; run the Makefile build target first" >&2
	exit 2
fi

target_dir="${CARGO_TARGET_DIR:-target}"
mkdir -p "$target_dir"
target_dir="$(cd "$target_dir" && pwd -P)"
config_home="$(mktemp -d "$target_dir/test-config.XXXXXX")"
cleanup() {
	rm -rf -- "$config_home"
}
trap cleanup EXIT

export XDG_CONFIG_HOME="$config_home"
./target/release/fcvm setup --generate-config --force >/dev/null

"$@"
