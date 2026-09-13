#!/usr/bin/env bash
# apt-get for self-hosted CI jobs: wait for another apt process to release its lock instead
# of failing on it.
#
# A self-hosted runner is a freshly booted instance, and its boot-time apt can still hold a
# lock when the first job starts. On 2026-09-13 Host-arm64 on #921 was assigned 74 s after
# its runner launched and failed in "Install dependencies" with
#   E: Could not get lock /var/lib/dpkg/lock-frontend. It is held by process 3180 (apt)
#
# DPkg::Lock::Timeout makes `apt-get install` wait for the dpkg frontend lock. It does not
# cover the lists lock that `apt-get update` takes. Measured with each lock held by another
# process:
#   install                              rc=100 in 0.0 s
#   install -o DPkg::Lock::Timeout=30    waited, rc=0 in 11.4 s
#   update                               rc=100 in 0.8 s
#   update  -o DPkg::Lock::Timeout=30    rc=100 in 0.8 s
# So the timeout is passed for the frontend lock, and a failure that reports a held lock is
# retried until APT_LOCK_WAIT seconds have passed. Any other failure, or a lock still held at
# the deadline, returns apt-get's own exit status. Each attempt is one whole apt-get run, so
# there is no window between checking a lock and taking it.
#
# Usage: scripts/ci-apt-get.sh <apt-get arguments>
# APT_GET, SUDO and APT_LOCK_RETRY_S exist for tests/test_ci_workflow_coverage.rs.
set -uo pipefail

wait_s=${APT_LOCK_WAIT:-600}
retry_s=${APT_LOCK_RETRY_S:-5}
apt_get=${APT_GET:-apt-get}
sudo_cmd=${SUDO-sudo}

out=$(mktemp) || exit 1
trap 'rm -f "$out"' EXIT
deadline=$((SECONDS + wait_s))
attempt=0
while :; do
  attempt=$((attempt + 1))
  left=$((deadline - SECONDS))
  [ "$left" -gt 0 ] || left=1
  $sudo_cmd "$apt_get" -o DPkg::Lock::Timeout="$left" "$@" 2>&1 | tee "$out"
  rc=${PIPESTATUS[0]}
  [ "$rc" -eq 0 ] && exit 0
  if grep -qE '^E: Could not get lock ' "$out" && [ "$SECONDS" -lt "$deadline" ]; then
    echo "ci-apt-get: attempt $attempt found an apt lock held by another process; retrying in ${retry_s}s ($((deadline - SECONDS))s left)" >&2
    sleep "$retry_s"
    [ "$SECONDS" -lt "$deadline" ] || exit "$rc"
    continue
  fi
  exit "$rc"
done
