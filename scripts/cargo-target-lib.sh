#!/usr/bin/env bash
# Shared Cargo-target generation protocol. This file is sourced by both the
# publisher and runner so the retirement xattr and its exit-code contract cannot
# drift between the two sides of the handoff.

# Query the retirement xattr through an already-locked directory fd. Exit 0
# means retired, 3 means current, and every other status is a fatal protocol
# error. The pruner persists this xattr before changing a single cache byte.
target_is_retired() {
	local fd="$1" rc=0
	/usr/bin/python3 -c '
import errno
import os
import sys

try:
    value = os.getxattr(sys.argv[1], b"user.fcvm.retired")
except OSError as error:
    if error.errno in (errno.ENODATA, getattr(errno, "ENOATTR", errno.ENODATA)):
        raise SystemExit(3)
    raise
if value != b"v1":
    print(f"unsupported retired-generation marker: {value!r}", file=sys.stderr)
    raise SystemExit(4)
' "/proc/$$/fd/$fd" || rc=$?
	return "$rc"
}
