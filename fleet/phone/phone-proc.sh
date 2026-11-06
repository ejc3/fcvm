# Process helpers that phone.sh and the GPU hook share; both source this file from their own
# directory (/usr/local/bin in the phone container, fleet/phone in their tests).

# A process that is running: a zombie (exited, not yet reaped, which happens to an orphan in
# a container whose first process reaps nothing) is not.
alive() { local st; st=$(sed 's/.*) //' "/proc/$1/stat" 2>/dev/null | cut -d' ' -f1) && [ -n "$st" ] && [ "$st" != Z ]; }
any_alive() { local p; for p in $(pgrep -x "$1"); do alive "$p" && return 0; done; return 1; }

# Stops the process $1 (a pid, or empty) and every process named $2, and waits until none of
# them runs. SIGTERM first; SIGKILL to what still runs STOP_TERM_MS later (a streamer stuck in
# a CUDA or NVENC teardown must not hang its caller, which holds streamer.lock); fails, naming
# what is left and its state, when something still runs STOP_KILL_MS after the SIGKILL (a
# process in uninterruptible sleep, which no signal ends).
stop_procs() {
  local pid=$1 name=$2 t0 now killed=0 p left
  if [ -n "$pid" ]; then kill "$pid" 2>/dev/null || true; fi
  pkill -x "$name" 2>/dev/null || true
  t0=$(date +%s%3N)
  while { [ -n "$pid" ] && alive "$pid"; } || any_alive "$name"; do
    now=$(date +%s%3N)
    if [ "$killed" = 0 ] && [ $(( now - t0 )) -ge "${STOP_TERM_MS:-5000}" ]; then
      echo "$name did not exit within ${STOP_TERM_MS:-5000} ms of SIGTERM: sending SIGKILL" >&2
      if [ -n "$pid" ]; then kill -9 "$pid" 2>/dev/null || true; fi
      pkill -9 -x "$name" 2>/dev/null || true
      killed=1
      t0=$now
    elif [ "$killed" = 1 ] && [ $(( now - t0 )) -ge "${STOP_KILL_MS:-5000}" ]; then
      left=""
      for p in $pid $(pgrep -x "$name"); do
        alive "$p" && left="$left $p ($(sed 's/.*) //' "/proc/$p/stat" 2>/dev/null | cut -d' ' -f1))"
      done
      echo "$name still runs ${STOP_KILL_MS:-5000} ms after SIGKILL:$left" >&2
      return 1
    fi
    sleep 0.05
  done
}
