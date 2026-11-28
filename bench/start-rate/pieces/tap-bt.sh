#!/usr/bin/env bash
# Stands in for perf as the driver's --perf-cmd: runs tap.bt over the phase the driver would
# have run `perf stat` over, writing next to the file perf was told to write (-o FILE).
# The driver stops it with SIGINT, which makes bpftrace print its maps.
out=
while [ $# -gt 0 ]; do
  if [ "$1" = -o ]; then out=$2; shift; fi
  shift
done
[ -n "$out" ] || { echo "tap-bt.sh: no -o FILE among the arguments" >&2; exit 2; }
exec sudo -n bpftrace -o "${out%.csv}.bt" "$(dirname "$0")/tap.bt"
