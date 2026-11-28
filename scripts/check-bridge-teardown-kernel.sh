#!/bin/bash
# Fail unless a kernel release carries Linux commit 25ae123db10b, which removes an
# rcu_barrier() from br_multicast_dev_del(). Without it the kernel runs one barrier per
# dying bridge under a single RTNL hold, and every network operation on the host waits
# for seconds while a batch of fcvm VMs stops (kernel/README.md, #1563, #1685).
#
#   check-bridge-teardown-kernel.sh [--accept-ubuntu-backport] [RELEASE]
#
# RELEASE is a `uname -r` string. With no RELEASE argument the running kernel is checked;
# an empty one fails, like any release this script cannot parse. A release is three numbers
# without leading zeros, then nothing or a suffix that starts with `-` or `+`, which is what
# `uname -r` prints (7.3.3, 7.3.3-fcvm-0123456789ab, 6.18.60+). `7.2` alone and `6.18.60.1`
# are not releases. The rule is the release's version alone:
#   6.18.x with x >= 60, or 7.2 and later: the fix is there.
#   any other release: treated as lacking it. The fix reached no other stable line
#   (6.19, 7.0 and 7.1 ended before it).
# --accept-ubuntu-backport also passes Ubuntu's AWS 7.0 kernel from 7.0.0-1017 on
# (`7.0.0-<abi>-aws`), whose changelog lists the commit. A box on a stock Ubuntu image
# passes that flag; the CI runners do not, because their image installs the repo's own
# host kernel.
set -euo pipefail

accept_ubuntu_backport=0
if [ "${1:-}" = "--accept-ubuntu-backport" ]; then
  accept_ubuntu_backport=1
  shift
fi
if [ "$#" -gt 1 ]; then
  echo "usage: $0 [--accept-ubuntu-backport] [RELEASE]" >&2
  exit 2
fi
if [ "$#" -eq 1 ]; then
  release="$1"
else
  release="$(uname -r)"
fi

fixed=0
number='(0|[1-9][0-9]*)'
if [[ $release =~ ^$number\.$number\.$number([-+].*)?$ ]]; then
  major=${BASH_REMATCH[1]}
  minor=${BASH_REMATCH[2]}
  patch=${BASH_REMATCH[3]}
  if [ "$major" -gt 7 ] || { [ "$major" -eq 7 ] && [ "$minor" -ge 2 ]; } ||
    { [ "$major" -eq 6 ] && [ "$minor" -eq 18 ] && [ "$patch" -ge 60 ]; }; then
    fixed=1
  fi
fi
if [ "$fixed" -eq 0 ] && [ "$accept_ubuntu_backport" -eq 1 ] &&
  [[ $release =~ ^7\.0\.0-([1-9][0-9]*)-aws$ ]] && [ "${BASH_REMATCH[1]}" -ge 1017 ]; then
  fixed=1
fi

if [ "$fixed" -eq 1 ]; then
  echo "kernel $release has the bridge teardown fix (Linux 25ae123db10b)"
  exit 0
fi
echo "ERROR: kernel '$release' lacks the bridge teardown fix (Linux 25ae123db10b, in 6.18.60 and" >&2
echo "       later 6.18 releases and in 7.2 and later): stopping VMs together stalls every" >&2
echo "       network operation on this host for seconds. Boot a kernel with the fix." >&2
exit 1
