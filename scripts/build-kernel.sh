#!/usr/bin/env bash
# Optional: build or fetch a kernel. For many hosts, copying an existing vmlinux is enough.
# Usage: build-kernel.sh <OUT_PATH>
set -euo pipefail

OUT_PATH="${1:-${HOME}/.local/share/fcvm/images/vmlinux}"
echo "[kernel] This is a placeholder. Provide your own vmlinux or implement a kernel build here."
echo "[kernel] When ready, place it at: ${OUT_PATH}"
