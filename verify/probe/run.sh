#!/usr/bin/env bash
# Build counter-replay.c and compare what it prints with expected.txt. Needs an arm64 host
# with /dev/kvm booted with kvm-arm.mode=nested (FEAT_NV2); without that the program exits 3
# after the plain cases and this script fails. Run by hand (`make verify-counter-probe`): it
# is a measurement of the host kernel, not a check of the model, and CI's hosted runners have
# no arm64 KVM.
set -euo pipefail
cd "$(dirname "$0")"
command -v cc >/dev/null 2>&1 || { echo "BLOCKED: no C compiler (cc)" >&2; exit 2; }
[ "$(uname -m)" = aarch64 ] || { echo "BLOCKED: arm64 only, this is $(uname -m)" >&2; exit 2; }
[ -r /dev/kvm ] && [ -w /dev/kvm ] || { echo "BLOCKED: /dev/kvm is not usable by this user" >&2; exit 2; }
mkdir -p build
cc -O1 -Wall -Wextra -Werror -o build/counter-replay counter-replay.c
echo "host kernel: $(uname -r)" >&2
./build/counter-replay > build/out.txt
diff expected.txt build/out.txt
echo "counter-replay: the host kernel does what expected.txt records" >&2
