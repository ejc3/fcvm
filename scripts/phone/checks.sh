# The shell setup and the checks scripts/phone/run.sh shares with its test
# (scripts/phone/test-run-checks.sh). Sourced; expects F (the fcvm binary).
set -euo pipefail
fail=0
ok() { echo "ok   $*"; }
bad() { echo "FAIL $*"; fail=1; }

# The lowest of the app's fps samples over the next 4 seconds on VM $1 ("" when it logged none).
lowest_fps() {
  "$F" exec --pid "$1" -- /usr/local/bin/phone.sh draws 4 2>&1 | grep -o 'fps=[0-9.]*' | cut -d= -f2 | sort -n | head -1
}
at_least_30() { python3 -c 'import sys; sys.exit(0 if sys.argv[1] and float(sys.argv[1]) >= 30 else 1)' "${1:-}"; }
# VM $1's probe app draws at least 30 frames a second; $2 says when ("after the retake").
check_draws() {
  local fps
  fps=$(lowest_fps "$1")
  at_least_30 "$fps" && ok "the app draws $2: lowest fps sample $fps" \
    || bad "the app does not draw 30 fps $2: lowest '${fps:-none}'"
}
