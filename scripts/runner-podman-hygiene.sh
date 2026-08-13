#!/usr/bin/env bash
# Remove the DEFAULT rootless podman store before tests run.
#
# CI never legitimately uses it: every job writes ~/.config/containers/storage.conf
# pointing the graphroot at /mnt/fcvm-btrfs, so any content under
# ~/.local/share/containers is a dropping by definition — which is what makes
# unconditional removal correct and heuristics unnecessary.
#
# Why it must be removed: the store arrives PRE-CONTAMINATED from the AMI.
# 8a9c564f switched AMI creation to snapshotting the RUNNING builder, so
# whatever rootless-store state the builder accumulated (partial layers,
# foreign-uid files from a different subuid map) is baked into every runner.
# The first `podman build` that resolves to the default store then dies with
#   chown .../storage/overlay/l: operation not permitted
# — which is how every Host job failed on 2026-08-12 (#792/#805). Tests were
# only steered into the default store by an XDG_CONFIG_HOME redirect that is
# also fixed (FCVM_CONFIG_DIR), so this is defence in depth: the droppings stay
# gone even if some future path resolves the default store again.
#
# Removal uses sudo because the droppings are, by nature, not ours to delete
# unprivileged — foreign-uid files are the failure mode.
set -euo pipefail

store="${HOME:?HOME must be set}/.local/share/containers"

if [ ! -e "$store" ]; then
	echo "runner-podman-hygiene: no default store at $store (clean)"
	exit 0
fi

# Record what was there before destroying the evidence — this is the data the
# AMI follow-up (issue #806) needs to confirm where the droppings come from.
echo "runner-podman-hygiene: removing pre-existing default store $store:"
sudo find "$store" -maxdepth 3 -printf '%u:%g %m %p\n' 2>/dev/null | head -20 || true

sudo rm -rf "$store"
echo "runner-podman-hygiene: removed"
