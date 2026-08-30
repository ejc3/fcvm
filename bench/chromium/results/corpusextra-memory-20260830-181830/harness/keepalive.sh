#!/bin/bash
# The idle watchdog (parallel-box-watchdog.tf) terminates this box when the
# 5-minute CPUUtilization maximum stays under 5% for 30 minutes. A 2-vCPU
# benchmark on a 192-core box is ~1%, so a measured run looks idle and the box
# is reaped mid-run (observed 2026-08-30 16:57:42, killing a campaign 20 s from
# its analysis). This raises one 5-minute bucket above the threshold and is run
# BETWEEN measured phases only: 96 of 192 cores for 120 s is ~10% of a bucket,
# and the phases that follow have their own quiet-box gates, which wait for the
# 1-minute load to decay before anything is measured.
CORES="${CORES:-96}"
SECS="${SECS:-120}"
for _ in $(seq 1 "$CORES"); do
    timeout "$SECS" sh -c 'while :; do :; done' >/dev/null 2>&1 &
done
wait
echo "keepalive: $CORES cores for ${SECS}s done at $(date +%H:%M:%S), load=$(cut -d' ' -f1 /proc/loadavg)"
