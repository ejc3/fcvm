#!/bin/bash
# The syscalls each libc function makes under glibc and under musl on this machine, as JSON:
# {"glibc": {function: [syscall, ...]}, "musl": {...}}. The confined seccomp policy is a
# musl-built Firecracker's filter run by a glibc-built Firecracker, so where the two libcs
# make different calls for one function the policy lists glibc's
# (confine::tests::glibcs_call_is_listed_wherever_the_fork_filter_lists_musls).
# Needs gcc, musl-gcc (musl-tools) and strace.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
gcc -O0 -pthread -o "$work/probe-glibc" "$here/probe.c"
musl-gcc -O0 -static -pthread -o "$work/probe-musl" "$here/probe.c"
(cd "$work" && strace -f -o glibc.strace ./probe-glibc && strace -f -o musl.strace ./probe-musl)
python3 "$here/table.py" "$work/glibc.strace" "$work/musl.strace"
