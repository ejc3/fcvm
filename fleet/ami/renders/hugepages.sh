#!/bin/bash
# Reserve pages mode's 2 MiB pages early in boot, before memory fragments
# (fcvm-fleet-hugepages.service). The count is RENDERS_HUGEPAGES in renders.conf. The kernel
# reserves what it can find and says so only in nr_hugepages: fewer than asked fails the unit,
# after memory is compacted and the reservation asked for again, so a host whose pool is short
# never starts the agent (fcvm-fleet-renders-agent.service requires this unit).
set -euo pipefail
RENDERS_CONF=${RENDERS_CONF:-/etc/fcvm-fleet/renders.conf}
POOL=${POOL:-/sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages}
COMPACT=${COMPACT:-/proc/sys/vm/compact_memory}
POOL_TRIES=${POOL_TRIES:-3}

pool_now() { tr -d '[:space:]' < "$POOL"; }
pool_ask() { echo "$1" > "$POOL"; }
compact() { echo 1 > "$COMPACT"; }

main() {
  local want got try
  # shellcheck source=renders.conf
  . "$RENDERS_CONF"
  want=${RENDERS_HUGEPAGES:?renders.conf sets no RENDERS_HUGEPAGES}
  [[ "$want" =~ ^[0-9]+$ ]] || { echo "fcvm-fleet-hugepages: RENDERS_HUGEPAGES=$want is not a number"; return 1; }
  for ((try = 1; try <= POOL_TRIES; try++)); do
    [ "$try" = 1 ] || compact
    pool_ask "$want"
    got=$(pool_now)
    [ "$got" = "$want" ] && { echo "fcvm-fleet-hugepages: $got 2 MiB pages reserved (try $try)"; return 0; }
  done
  echo "fcvm-fleet-hugepages: the kernel reserved $got of $want 2 MiB pages after $POOL_TRIES tries"
  return 1
}

[ -n "${HUGEPAGES_LIB:-}" ] || main
