#!/bin/bash
# Log when the renderer serves its first frame, as milliseconds since the kernel started
# (CLOCK_BOOTTIME); with the instance's launch time this gives launch to first frame.
for _ in $(seq 1 6000); do
  if curl -fsS -m 1 http://127.0.0.1:8080/health 2>/dev/null | jq -e '.ready == true and .frame > 0' >/dev/null 2>&1; then
    up=$(awk '{printf "%d", $1 * 1000}' /proc/uptime)
    echo "first_frame boot_ms=$up at=$(date -u +%FT%T.%3NZ)" | tee /var/log/fcvm-fleet-first-frame.log
    exit 0
  fi
  sleep 0.1
done
echo "no first frame in 10 minutes"; exit 1
