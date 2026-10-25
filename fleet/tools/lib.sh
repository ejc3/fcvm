# Shell helpers for the fleet/tools run scripts (sourced, bash).

# wait_listening HOST PORT SECONDS [PID]: return once something accepts TCP connections
# at HOST:PORT, or fail after SECONDS, or as soon as process PID (the server being waited
# for) has exited. A coordinator can take more than a fixed sleep to bind (it reads its
# state file first), and the clients the scripts start next do not retry.
wait_listening() {
  local host=$1 port=$2 secs=$3 pid=${4:-} end
  end=$(( $(date +%s) + secs ))
  while ! (exec 3<>"/dev/tcp/$host/$port") 2>/dev/null; do
    if [ -n "$pid" ] && ! kill -0 "$pid" 2>/dev/null; then
      echo "process $pid exited before $host:$port listened" >&2
      return 1
    fi
    if [ "$(date +%s)" -ge "$end" ]; then
      echo "nothing listens on $host:$port after $secs s" >&2
      return 1
    fi
    sleep 0.2
  done
}
