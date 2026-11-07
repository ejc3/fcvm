#!/bin/bash
# Build pjdfstest at $PJDFSTEST_DIR (default /tmp/pjdfstest-check), where the fuse-pipe POSIX
# tests run it. Called by `make setup-pjdfstest`.
#
# Several makes on one host can run this at once (other worktrees, the root and non-root
# lanes). The recipe this replaced removed and cloned into the shared path itself, and a second
# make's `rm -rf` under a running clone failed it with "cannot copy .../hooks/
# push-to-checkout.sample". Here the check, the build and the install run under one flock, and
# the build happens in a private directory renamed into place, so a reader never sees a
# half-built tree.
#
# The lock is the destination's parent directory, opened read-only: a lock file of its own in
# /tmp would be refused to every user but its owner under fs.protected_regular.
#
# PJDFSTEST_REPO and PJDFSTEST_BUILD exist for tests/test_setup_pjdfstest.rs.
set -euo pipefail
dest=${PJDFSTEST_DIR:-/tmp/pjdfstest-check}
repo=${PJDFSTEST_REPO:-https://github.com/pjd/pjdfstest}
build=${PJDFSTEST_BUILD:-autoreconf -ifs && ./configure && make}

[ -x "$dest/pjdfstest" ] && exit 0
exec 9<"$(dirname "$dest")"
flock 9
[ -x "$dest/pjdfstest" ] && exit 0
echo '==> Building pjdfstest...'
work=$(mktemp -d "$dest.build.XXXXXX")
trap 'rm -rf "$work"' EXIT
git clone -q --depth 1 "$repo" "$work/src"
(cd "$work/src" && sh -c "$build")
# A directory without the binary (left by the old recipe) is moved aside rather than removed:
# a test could still be reading it.
if [ -e "$dest" ]; then
    mv -T "$dest" "$(mktemp -u "$dest.stale.XXXXXX")"
fi
mv -T "$work/src" "$dest"
