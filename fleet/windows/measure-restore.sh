#!/bin/bash
# Measure the Windows sandboxes' restore on this host's fcvm and the Cloud Hypervisor it runs:
# a gold snapshot made the way the fleet agent makes it (cold boot, log on through the bridge
# twice with the desktop idle after each, snapshot and stop), then N clones restored at once.
# For each N it prints the time from `fcvm snapshot run` to healthy (the guest agent has set the
# clock and Remote Desktop answers), each clone's memory 20 s later, whether its VMM maps the
# snapshot's memory file or holds a copy of it, the VMMs' CPU use, and how long the bridge takes
# to reach the restored desktop. docs/fleet/windows-sandboxes.md, "Measurements", cites it.
#
#   measure-restore.sh [OUT_DIR]     results in OUT_DIR (default ./windows-restore-<time>):
#                                    summary.txt, results.json, a log per VM, the desktops as PPM
#
# The image is build-image.sh's (every input pinned by SHA-256 there); it is built if absent.
# Run it as the user that runs fcvm, after `make build setup-fcvm setup-cloud-hypervisor` and
# `make -C fleet/windows bridge-image`. Nothing else may be running VMs on the host.
#
#   FCVM          the fcvm binary (default: target/release/fcvm of this checkout)
#   IMAGE_DIR     an image directory to use instead of building one
#   BRIDGE_IMAGE  the bridge's container image (default: the one `make bridge-image` tags)
#   COUNTS        how many clones to restore at once, in turn (default "1 4 8")
#   AGE_SECS      seconds to wait between the snapshot and the first restore (default 0)
#   GOLD_TAG      restore this existing snapshot instead of making one
#   KEEP_GOLD=1   leave the gold snapshot in place afterwards
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
FCVM=${FCVM:-$REPO/target/release/fcvm}
COUNTS=${COUNTS:-1 4 8}
AGE_SECS=${AGE_SECS:-0}
OUT=$(realpath -m "${1:-windows-restore-$(date -u +%Y%m%dT%H%M%SZ)}")
CPUS=2
MEM_MIB=4096
# The fleet agent's own bounds (src/fleet/agent/windows.rs).
IDLE_SHARE=0.25
IDLE_WINDOW=20
IDLE_DEADLINE=300
HEALTHY_DEADLINE=600
SETTLE_SECS=20

say() { echo "measure-restore: $*" >&2; }
die() { say "$*"; exit 1; }
for tool in jq podman awk make; do
  command -v "$tool" >/dev/null || die "'$tool' is missing"
done
[ -x "$FCVM" ] || die "no fcvm at $FCVM (make build)"
mkdir -p "$OUT"

IMG=${IMAGE_DIR:-$("$HERE/build-image.sh")}
[ -s "$IMG/manifest.json" ] || die "$IMG is not an image directory"
BRIDGE_IMAGE=${BRIDGE_IMAGE:-localhost/fcvm-windows-bridge:$(make -s -C "$HERE" bridge-image-id)}
podman image exists "$BRIDGE_IMAGE" || die "no bridge image $BRIDGE_IMAGE (make -C fleet/windows bridge-image)"

RUN=wm$$
STARTED=()   # fcvm pids this run started
cleanup() {
  local pid
  for pid in "${STARTED[@]:-}"; do [ -n "$pid" ] && kill -TERM "$pid" 2>/dev/null || true; done
  for pid in "${STARTED[@]:-}"; do [ -n "$pid" ] && wait "$pid" 2>/dev/null || true; done
  podman rm -f "$RUN-bridge" >/dev/null 2>&1 || true
  if [ -n "${MADE_GOLD:-}" ] && [ -z "${KEEP_GOLD:-}" ]; then
    "$FCVM" snapshots delete --force "$MADE_GOLD" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

now() { date +%s.%N; }
since() { awk -v a="$1" -v b="$(now)" 'BEGIN { printf "%.2f", b - a }'; }

# One field of the `fcvm ls --json` entry whose fcvm process is $1.
ls_field() {
  "$FCVM" ls --json 2>/dev/null | jq -r --argjson pid "$1" "[.[] | select(.pid == \$pid) | $2][0] // empty"
}

# Wait until the VM of fcvm process $1 is healthy; fail if fcvm exits or $2 seconds pass.
wait_healthy() {
  local pid=$1 deadline=$2 t0
  t0=$(now)
  while :; do
    [ "$(ls_field "$pid" .health_status)" = healthy ] && return 0
    kill -0 "$pid" 2>/dev/null || die "fcvm $pid exited before its VM was healthy"
    awk -v s="$(since "$t0")" -v d="$deadline" 'BEGIN { exit !(s > d) }' && die "fcvm $pid: not healthy after $deadline s"
    sleep 0.1
  done
}

# The cloud-hypervisor process under fcvm process $1 (the kernel cuts its name to 15 bytes).
vmm_of() {
  ps -e -o pid=,ppid=,comm= | awk -v root="$1" '
    { parent[$1] = $2; name[$1] = $3 }
    END {
      for (p in name) if (name[p] ~ /^cloud-hyperviso/) {
        for (q = p; q != "" && q != 0; q = parent[q]) if (q == root) { print p; exit }
      }
    }'
}

# CPU seconds (user + system) a process has used.
cpu_secs() { awk -v hz="$(getconf CLK_TCK)" '{ sub(/^.*\) /, ""); printf "%.3f", ($12 + $13) / hz }' "/proc/$1/stat"; }

# Share of one core process $1 uses over $2 seconds.
cpu_share() {
  local a b
  a=$(cpu_secs "$1"); sleep "$2"; b=$(cpu_secs "$1")
  awk -v a="$a" -v b="$b" -v s="$2" 'BEGIN { printf "%.3f", (b - a) / s }'
}

# Wait until VMM $1 averages under IDLE_SHARE of a core over IDLE_WINDOW seconds.
wait_idle() {
  local vmm=$1 t0 share
  t0=$(now)
  while :; do
    share=$(cpu_share "$vmm" "$IDLE_WINDOW")
    awk -v s="$share" -v t="$IDLE_SHARE" 'BEGIN { exit !(s < t) }' && { echo "$share"; return 0; }
    awk -v s="$(since "$t0")" -v d="$IDLE_DEADLINE" 'BEGIN { exit !(s > d) }' \
      && die "the VMM still used $share of a core after $IDLE_DEADLINE s"
  done
}

# Log on through the bridge and wait for the settled desktop, saved as $2; prints the seconds.
logon() {
  local sock=$1 ref=$2 t0 dir
  dir=$(dirname "$ref")
  mkdir -p "$dir"
  rm -f "$ref"
  t0=$(now)
  podman run --rm --replace --pull=never --name "$RUN-bridge" \
    --volume "$sock:/run/windows/rdp.sock" \
    --volume "$IMG/sandbox-password:/run/windows/sandbox-password:ro" \
    --volume "$dir:/run/windows/state" \
    --env RDP_SOCKET=/run/windows/rdp.sock --env RDP_USER=sandbox \
    --env RDP_PASSWORD_FILE=/run/windows/sandbox-password \
    --env "DESKTOP_REF_OUT=/run/windows/state/$(basename "$ref")" \
    "$BRIDGE_IMAGE" logon >>"$OUT/bridge.log" 2>&1 || die "the bridge's logon failed (see $OUT/bridge.log)"
  [ -s "$ref" ] || die "the bridge saved no desktop at $ref"
  since "$t0"
}

# "rss pss private_dirty" of process $1, in MiB.
memory_of() {
  awk '/^Rss:/ { r = $2 } /^Pss:/ { p = $2 } /^Private_Dirty:/ { d = $2 }
       END { printf "%d %d %d", r / 1024, p / 1024, d / 1024 }' "/proc/$1/smaps_rollup"
}

# Whether process $1 maps a snapshot's memory file ("mapped") or not ("copied").
mapping_of() {
  if grep -Eq '/memory-ranges( \(deleted\))?$' "/proc/$1/maps"; then echo mapped; else echo copied; fi
}

SUMMARY=$OUT/summary.txt
ROWS=$OUT/rows.jsonl
: >"$ROWS"
note() { echo "$*" | tee -a "$SUMMARY"; }

note "host: $(uname -m), kernel $(uname -r), $(nproc) CPUs"
note "fcvm: $("$FCVM" --version 2>/dev/null | head -n 1)"
note "image: $(jq -r '"\(.id), \(.edition) \(.build), installed \(.installed_at)"' "$IMG/manifest.json")"
note "guest: $CPUS vCPUs, $MEM_MIB MiB"

GOLD=${GOLD_TAG:-}
if [ -z "$GOLD" ]; then
  GOLD=$RUN-gold
  t0=$(now)
  "$FCVM" vm run --name "$RUN-gold-vm" --firmware "$IMG/CLOUDHV.fd" --disk "$IMG/disk.raw" \
    --cpu "$CPUS" --mem "$MEM_MIB" --guest windows --health-port 3389 >"$OUT/gold-vm.log" 2>&1 &
  gold=$!
  STARTED+=("$gold")
  wait_healthy "$gold" "$HEALTHY_DEADLINE"
  note "gold: cold boot to healthy $(since "$t0") s"
  sock=$(ls_field "$gold" .config.rdp_socket)
  [ -S "$sock" ] || die "the gold VM lists no rdp socket"
  vmm=$(vmm_of "$gold")
  [ -n "$vmm" ] || die "no cloud-hypervisor process under fcvm $gold"
  vmm_version=$("$(readlink "/proc/$vmm/exe")" --version)
  note "cloud hypervisor: $vmm_version"
  # Assigned first: a failure inside $(...) in an argument would not stop the script.
  secs=$(logon "$sock" "$OUT/desktop-gold-1.ppm")
  note "gold: first logon to a settled desktop $secs s"
  share=$(wait_idle "$vmm")
  note "gold: idle at $share of a core"
  secs=$(logon "$sock" "$OUT/desktop-gold.ppm")
  note "gold: second logon $secs s"
  share=$(wait_idle "$vmm")
  note "gold: idle at $share of a core"
  t0=$(now)
  "$FCVM" snapshot create --pid "$gold" --tag "$GOLD" --stop >"$OUT/gold-snapshot.log" 2>&1 \
    || die "snapshot create failed (see $OUT/gold-snapshot.log)"
  MADE_GOLD=$GOLD
  note "gold: snapshot create --stop $(since "$t0") s (tag $GOLD)"
  wait "$gold" 2>/dev/null || true
fi

[ "$AGE_SECS" = 0 ] || { say "waiting $AGE_SECS s before the restores"; sleep "$AGE_SECS"; }

note ""
note "$(printf '%-3s %-22s %-17s %-17s %-19s %-8s %-10s %s' N 'to healthy (s)' 'RSS (MiB)' 'PSS (MiB)' 'private dirty (MiB)' memory 'CPU share' 'desktop (s)')"
for n in $COUNTS; do
  pids=(); starts=(); healthy=()
  for i in $(seq 1 "$n"); do
    starts+=("$(now)")
    "$FCVM" snapshot run --snapshot "$GOLD" --name "$RUN-c$n-$i" >"$OUT/clone-$n-$i.log" 2>&1 &
    pids+=("$!")
    STARTED+=("$!")
    healthy+=("")
  done
  deadline_t0=$(now)
  left=$n
  while [ "$left" -gt 0 ]; do
    listing=$("$FCVM" ls --json 2>/dev/null || echo '[]')
    for i in $(seq 0 $((n - 1))); do
      [ -z "${healthy[$i]}" ] || continue
      if [ "$(jq -r --argjson pid "${pids[$i]}" '[.[] | select(.pid == $pid) | .health_status][0] // empty' <<<"$listing")" = healthy ]; then
        healthy[$i]=$(since "${starts[$i]}")
        left=$((left - 1))
      else
        kill -0 "${pids[$i]}" 2>/dev/null || die "clone ${pids[$i]} exited before it was healthy (see $OUT/clone-$n-$((i + 1)).log)"
      fi
    done
    awk -v s="$(since "$deadline_t0")" -v d="$HEALTHY_DEADLINE" 'BEGIN { exit !(s > d) }' && die "$left of $n clones not healthy after $HEALTHY_DEADLINE s"
    sleep 0.05
  done
  sleep "$SETTLE_SECS"
  rss=(); pss=(); dirty=(); maps=(); vmms=()
  for i in $(seq 0 $((n - 1))); do
    vmm=$(vmm_of "${pids[$i]}")
    [ -n "$vmm" ] || die "no cloud-hypervisor process under clone ${pids[$i]}"
    vmms+=("$vmm")
    mem=$(memory_of "$vmm")
    kind=$(mapping_of "$vmm")
    read -r r p d <<<"$mem"
    rss+=("$r"); pss+=("$p"); dirty+=("$d"); maps+=("$kind")
  done
  a=0; for v in "${vmms[@]}"; do a=$(awk -v a="$a" -v b="$(cpu_secs "$v")" 'BEGIN { print a + b }'); done
  sleep 5
  b=0; for v in "${vmms[@]}"; do b=$(awk -v a="$b" -v b="$(cpu_secs "$v")" 'BEGIN { print a + b }'); done
  share=$(awk -v a="$a" -v b="$b" -v n="$n" 'BEGIN { printf "%.3f", (b - a) / 5 / n }')
  sock=$(ls_field "${pids[0]}" .config.rdp_socket)
  desktop=$(logon "$sock" "$OUT/desktop-clone-$n.ppm")
  range() { printf '%s\n' "$@" | sort -g | awk 'NR == 1 { lo = $1 } { hi = $1 } END { if (lo == hi) print lo; else print lo " to " hi }'; }
  kinds=$(printf '%s\n' "${maps[@]}" | sort -u | paste -sd/)
  note "$(printf '%-3s %-22s %-17s %-17s %-19s %-8s %-10s %s' "$n" "$(range "${healthy[@]}")" "$(range "${rss[@]}")" "$(range "${pss[@]}")" "$(range "${dirty[@]}")" "$kinds" "$share" "$desktop")"
  jq -cn --argjson n "$n" --arg healthy "${healthy[*]}" --arg rss "${rss[*]}" --arg pss "${pss[*]}" \
    --arg dirty "${dirty[*]}" --arg maps "${maps[*]}" --arg share "$share" --arg desktop "$desktop" \
    '{n: $n, to_healthy_s: ($healthy | split(" ") | map(tonumber)), rss_mib: ($rss | split(" ") | map(tonumber)),
      pss_mib: ($pss | split(" ") | map(tonumber)), private_dirty_mib: ($dirty | split(" ") | map(tonumber)),
      memory: ($maps | split(" ")), cpu_share_per_clone: ($share | tonumber), desktop_s: ($desktop | tonumber)}' >>"$ROWS"
  for pid in "${pids[@]}"; do kill -TERM "$pid" 2>/dev/null || true; done
  for pid in "${pids[@]}"; do wait "$pid" 2>/dev/null || true; done
done

jq -n --arg kernel "$(uname -r)" --arg arch "$(uname -m)" --arg image "$(jq -r .id "$IMG/manifest.json")" \
  --argjson cpus "$CPUS" --argjson mem_mib "$MEM_MIB" --argjson age_secs "$AGE_SECS" --slurpfile rows "$ROWS" \
  '{kernel: $kernel, arch: $arch, image: $image, vcpus: $cpus, mem_mib: $mem_mib, age_secs: $age_secs, restores: $rows}' \
  >"$OUT/results.json"
rm -f "$ROWS"
if grep -q copied "$OUT/results.json"; then
  die "a clone holds a copy of the guest's memory instead of mapping the snapshot's file"
fi
say "results in $OUT"
