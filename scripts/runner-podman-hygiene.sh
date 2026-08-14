#!/usr/bin/env bash
# Per-job podman hygiene for self-hosted runners. Two independent repairs, both
# safe to run unconditionally because CI's podman state is disposable while its
# IMAGE LAYERS are the expensive persistent cache:
#
# 1. Remove the DEFAULT rootless store (~/.local/share/containers). CI never
#    legitimately uses it: every job writes ~/.config/containers/storage.conf
#    pointing the graphroot at /mnt/fcvm-btrfs, so any content there is a
#    dropping by definition. It arrives PRE-CONTAMINATED from the AMI: 8a9c564f
#    switched AMI creation to snapshotting the RUNNING builder, so whatever
#    rootless-store state the builder accumulated (partial layers, foreign-uid
#    files from a different subuid map) is baked into every runner. The first
#    `podman build` that resolves to it dies with
#      chown .../storage/overlay/l: operation not permitted
#    — how every Host job failed on 2026-08-12 (#792/#805). Removal needs sudo
#    because foreign-uid files are the failure mode.
#
# 2. Remove podman's state databases from the CONFIGURED graphroot, KEEPING the
#    image layers. The graphroot lives on /mnt/fcvm-btrfs, which persists
#    across runner instances, so a database poisoned by one job poisons every
#    later job on that volume: observed 2026-08-13, a db recording static dir
#    "" made every podman call fail with
#      database static dir "" does not match our static dir ".../libpod"
#    (exit 125 before any test ran) — and `podman system reset` REFUSES on
#    exactly that mismatch, so no podman-native repair can run. Deleting only
#    libpod/ and db.sql lets the next podman call recreate a coherent db while
#    the overlay layer cache (the one-time-cost content this volume exists to
#    keep) survives. An earlier version of this script used
#    `podman system reset --force` instead: on poisoned runners it failed the
#    same way as everything else, and on healthy ones it threw away the entire
#    persistent image cache every job.
#
# Invoked by ci.yml immediately after each job's `podman system migrate` — the
# last line of its podman configuration — so the graphroot parsed here is the
# one every later podman call will use.
set -euo pipefail

# The default-store sweep can escalate to sudo, so refuse to run against a
# HOME that is not this user's real home: a preceding step exporting a stray
# HOME would otherwise aim `sudo rm -rf` at an arbitrary directory. Tests
# exercise the script against a fixture home via FCVM_HYGIENE_HOME_OVERRIDE=1
# (fixture trees are user-owned, so those runs never reach sudo).
if [ "${FCVM_HYGIENE_HOME_OVERRIDE:-0}" != 1 ]; then
	runner_home="$(getent passwd "$(id -u)" | cut -d: -f6)"
	if [ -z "$runner_home" ] || [ "${HOME:-}" != "$runner_home" ]; then
		echo "runner-podman-hygiene: HOME (${HOME:-unset}) does not match this user's passwd home ($runner_home); refusing" >&2
		exit 1
	fi
fi
case "${HOME:-}" in
	/*) ;;
	*) echo "runner-podman-hygiene: HOME must be an absolute path" >&2; exit 1 ;;
esac

# Unprivileged removal first, sudo only for what it could not delete (the
# AMI's foreign-uid droppings). Keeps the script runnable where sudo is
# unavailable — the unprivileged test suite shims sudo with a hard failure,
# which is how the first version of this fallback was caught (2026-08-13:
# green under bare cargo-nextest, red under `make test-fast`'s sudo guard).
remove_tree() {
	rm -rf "$1" 2>/dev/null || sudo rm -rf "$1"
}

store="${HOME:?HOME must be set}/.local/share/containers"

# Record what is there before destroying the evidence — this is the data the
# AMI follow-up (issue #806) needs to confirm where the droppings come from.
if [ -e "$store" ]; then
	echo "runner-podman-hygiene: pre-existing default store $store:"
	find "$store" -maxdepth 3 -printf '%u:%g %m %p\n' 2>/dev/null | head -20 || true
fi
remove_tree "$store"
echo "runner-podman-hygiene: default store cleared"

conf="${HOME}/.config/containers/storage.conf"
graphroot=""
if [ -r "$conf" ]; then
	graphroot=$(sed -n 's/^graphroot[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' "$conf" | head -1)
fi
if [ -n "$graphroot" ] && [ -d "$graphroot" ]; then
	echo "runner-podman-hygiene: clearing state db under $graphroot (image layers preserved)"
	remove_tree "$graphroot/libpod"
	remove_tree "$graphroot/db.sql"
else
	echo "runner-podman-hygiene: no configured graphroot to heal (conf=$conf)"
fi
