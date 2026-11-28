#!/usr/bin/env bash
# Write perf-top.txt (top self-time frames, idle excluded) and perf-comm.txt into each cell
# under ROOT that has a perf.data. Usage: perftop.sh ROOT [LIMIT]
set -uo pipefail
root=$1
limit=${2:-25}
for data in "$root"/*/perf.data; do
  cell=$(dirname "$data")
  [ -s "$cell/perf-top.txt" ] && continue
  # Self time by command and symbol; the idle loop (swapper's default_idle_call, cpu_do_idle)
  # is not work.
  sudo perf report -i "$data" --no-children --sort comm,sym --stdio -g none --percent-limit 0.3 2>/dev/null \
    | grep -v '^#' | grep -v '^$' | grep -vE 'swapper|cpu_do_idle|default_idle_call|arch_cpu_idle|do_idle' \
    | head -n "$limit" | awk '{$1=$1; print}' > "$cell/perf-top.txt"
  sudo perf report -i "$data" --no-children --sort comm --stdio -g none 2>/dev/null \
    | grep -v '^#' | grep -v '^$' | head -n 20 > "$cell/perf-comm.txt"
done
