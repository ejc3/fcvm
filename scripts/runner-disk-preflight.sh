#!/usr/bin/env bash
# Disk-capacity preflight + cleanup for fcvm CI runners.
#
# One script, two invocation layers (2026-08-06: runner-i-0ea78b39da373fcca
# filled its disk and killed every job it picked up at "Set up job" —
# "No space left on device: /opt/actions-runner/_diag/..." — until it was
# manually deregistered; neither layer alone can prevent a repeat):
#
#   1. ci.yml — first step after checkout in every self-hosted job. Frees
#      space before the heavy steps and fails LOUDLY (naming the runner) if
#      the disk cannot be recovered, so a sick runner produces one clear
#      failure instead of a cascade of mystery ENOSPC errors.
#
#   2. runner-disk-guard.timer — hourly systemd timer on the runner itself
#      (installed by scripts/build-ami.sh and scripts/setup-runner.sh),
#      running with --quarantine. A job step cannot rescue a runner so full
#      that "Set up job" itself fails (the job dies before any step runs), so
#      when the hard floor cannot be met the guard stops the actions-runner
#      service: the box goes offline and the autoscaler replaces it, instead
#      of the runner accepting and poisoning every queued job.
#
# Thresholds (env-overridable):
#   MIN_FREE_GB      below this on any monitored fs, run cleanup (default 15)
#   HARD_FLOOR_GB    below this after cleanup → fail / quarantine (default 5)
#   LOG_AGE_DAYS     test logs / core dumps older than this (default 1)
#   TARGET_AGE_DAYS  cargo target dirs idle longer than this (default 3)
#   CACHE_AGE_DAYS   content-addressed cache entries idle longer (default 4)
#   DRY_RUN=1        print every candidate, delete nothing; runs all stages
#                    regardless of thresholds so the candidate list is auditable
#
# Cleanup stages run in order of increasing cost-to-recreate, and stop as soon
# as every monitored filesystem is back above MIN_FREE_GB:
#   1. podman system prune (rootless + root storage) — images/containers/
#      volumes are re-pulled or re-built on demand.
#   2. /tmp/fcvm-test-logs entries older than LOG_AGE_DAYS.
#   3. managed cargo/nextest generations under /mnt/fcvm-btrfs/cargo-target/
#      with no activity for TARGET_AGE_DAYS. Pre-protocol real target dirs in
#      runner _work are reported but retained because they cannot be atomically
#      rotated without renaming a possibly-mounted dentry.
#   4. content-addressed caches on /mnt/fcvm-btrfs (image-cache entries,
#      snapshot dirs, kernel/rootfs/initrd/firecracker assets) idle for
#      CACHE_AGE_DAYS, plus core dumps older than LOG_AGE_DAYS.
#
# "Idle" means neither modified NOR read since the cutoff: the btrfs volume is
# mounted relatime, so content-addressed assets the current checkout's SHAs
# actually reference are re-read every CI run and keep a fresh atime — they are
# never candidates. A pruned entry is regenerated on the next cache miss.
#
# Never touched: state/ (live VM state), vm-disks/ (possibly-attached VM
# disks), containers/ (podman's own store — managed via podman prune), and
# *.lock files (unlinking a lock file a process holds leaves two "owners" of
# the same critical section — the ABA locking bug class AGENTS.md bans).
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
TARGET_PRUNE_HELPER="$SCRIPT_DIR/prune-cargo-target.sh"

MIN_FREE_GB="${MIN_FREE_GB:-15}"
HARD_FLOOR_GB="${HARD_FLOOR_GB:-5}"
LOG_AGE_DAYS="${LOG_AGE_DAYS:-1}"
TARGET_AGE_DAYS="${TARGET_AGE_DAYS:-3}"
CACHE_AGE_DAYS="${CACHE_AGE_DAYS:-4}"
DRY_RUN="${DRY_RUN:-0}"

QUARANTINE=0
TARGET_DIRS_ONLY=0
for arg in "$@"; do
  case "$arg" in
    --quarantine) QUARANTINE=1 ;;
    # Operationally useful on a runner and a narrow entry point for the
    # concurrency regression tests.  It runs as the caller and touches only
    # the configured cargo target roots.
    --target-dirs-only) TARGET_DIRS_ONLY=1 ;;
    *)
      echo "usage: $0 [--quarantine] [--target-dirs-only]" >&2
      exit 2
      ;;
  esac
done

GIB=$((1024 * 1024 * 1024))
BTRFS_ROOT="${BTRFS_ROOT:-/mnt/fcvm-btrfs}"
RUNNER_WORK_ROOT="${RUNNER_WORK_ROOT:-/opt/actions-runner/_work}"

# All logging goes to stderr: several stages pipe NUL-delimited candidate
# paths between functions on stdout, and log lines must never enter that data
# stream.
log() { printf '[disk-preflight] %s\n' "$*" >&2; }
human() { numfmt --to=iec-i --suffix=B "$1"; }

# Root is required for most cleanup targets (root-owned test debris, root
# podman storage). CI runners and dev boxes both have passwordless sudo; the
# probe's stderr is suppressed only because we replace it with our own loud
# error right below.
if ((TARGET_DIRS_ONLY)); then
  # This mode is deliberately least-privilege: tests and operators point it at
  # caller-owned roots.  The normal preflight still elevates for root-owned VM
  # debris and caches.
  SUDO=()
elif [[ $EUID -eq 0 ]]; then
  SUDO=()
else
  SUDO=(sudo -n)
  if ! sudo -n true 2>/dev/null; then
    log "ERROR: passwordless sudo unavailable; cannot clean root-owned files"
    exit 1
  fi
fi

# Monitored filesystems: the runner install dir (where _diag lives — the fs
# that killed jobs at "Set up job"), the fcvm asset volume, and /tmp, deduped
# down to unique mountpoints. Paths missing on this host (e.g. no runner
# install, no btrfs volume) are simply not monitored.
MON=()
declare -A _seen_mp=()
for p in / /opt/actions-runner "$BTRFS_ROOT" /tmp; do
  [[ -e $p ]] || continue
  mp="$(findmnt -no TARGET -T "$p")"
  if [[ -z ${_seen_mp[$mp]:-} ]]; then
    _seen_mp[$mp]=1
    MON+=("$mp")
  fi
done

avail_bytes() { df -B1 --output=avail "$1" | tail -n1 | tr -d ' '; }

# The root/container data dirs are root-owned and not traversable by the CI
# user, so a plain [[ -d ]] reports "missing" and silently skips the largest
# cache on the box. Always test through sudo.
dir_exists() { "${SUDO[@]}" test -d "$1"; }

total_avail() {
  local sum=0 mp
  for mp in "${MON[@]}"; do
    sum=$((sum + $(avail_bytes "$mp")))
  done
  printf '%s\n' "$sum"
}

# any_below <gb>: true if any monitored fs has less than <gb> GiB free
any_below() {
  local floor_bytes=$(($1 * GIB)) mp
  for mp in "${MON[@]}"; do
    if (($(avail_bytes "$mp") < floor_bytes)); then
      return 0
    fi
  done
  return 1
}

df_report() { df -h "${MON[@]}" >&2; }

TARGET_CUTOFF="$(date -d "$TARGET_AGE_DAYS days ago" '+%Y-%m-%d %H:%M:%S')"
CACHE_CUTOFF="$(date -d "$CACHE_AGE_DAYS days ago" '+%Y-%m-%d %H:%M:%S')"

# recently_used <path> <cutoff>: was any FILE under <path> written (mtime) or
# read (atime; the volume is mounted relatime) since <cutoff>?
#
# -type f is load-bearing, not an optimization: reading a directory's entries
# updates that directory's atime, so this probe (and any du/ls/find anyone
# runs) would refresh the very timestamps it reads, and after one preflight
# run nothing would ever look idle again. File atimes are untouched by
# directory traversal, so they are the only self-consistent signal here.
#
# On probe failure we report the error and claim "in use" — failing toward
# keeping data, never toward deleting it.
recently_used() {
  local found
  if ! found="$("${SUDO[@]}" find "$1" -type f \( -newermt "$2" -o -newerat "$2" \) -print -quit 2>&1)"; then
    log "  WARNING: activity probe failed for $1 ($found); treating as in-use"
    return 0
  fi
  [[ -n $found ]]
}

# delete_candidates <label>: read NUL-delimited paths on stdin, log each with
# its size, delete unless DRY_RUN. Sizes are apparent allocation (du); btrfs
# reflinks can make the actually-freed amount smaller — the per-stage df delta
# logged by main() shows the real effect.
delete_candidates() {
  local label="$1" total=0 count=0 path size verb="freed"
  if ((DRY_RUN)); then
    verb="would be freed"
  fi
  while IFS= read -r -d '' path; do
    # A candidate can legitimately vanish between listing and deletion
    # (e.g. a concurrent job's own cleanup); that is not an error.
    [[ -e $path ]] || continue
    size="$("${SUDO[@]}" du -sb -- "$path" | awk '{print $1}')" || return 1
    if ((DRY_RUN)); then
      log "  would delete: $path ($(human "$size"))"
    else
      "${SUDO[@]}" rm -rf -- "$path" || return 1
      log "  deleted: $path ($(human "$size"))"
    fi
    total=$((total + size))
    count=$((count + 1))
  done
  log "[$label] $count entries, $(human "$total") $verb"
}

# Stage 1: podman images, stopped containers, volumes. Everything podman
# stores is re-pulled or re-built on demand (localhost/nested-test is
# auto-rebuilt by ensure_nested_image()).
stage_podman_prune() {
  if ! command -v podman >/dev/null 2>&1; then
    log "podman not installed; nothing to prune"
    return 0
  fi
  local users=() u
  if [[ $EUID -eq 0 ]]; then
    # Guard mode (systemd): root storage plus the runner user's rootless
    # storage (CI configures its graphroot on the btrfs volume).
    users=(root)
    if [[ -d /opt/actions-runner ]]; then
      u="$(stat -c %U /opt/actions-runner)"
      if [[ $u != root ]]; then
        users+=("$u")
      fi
    fi
  else
    users=("$(id -un)" root)
  fi
  for u in "${users[@]}"; do
    if ((DRY_RUN)); then
      log "would run 'podman system prune -af --volumes' as $u; current usage:"
      if [[ $u == "$(id -un)" ]]; then
        podman system df >&2 || return 1
      else
        "${SUDO[@]}" podman system df >&2 || return 1
      fi
      continue
    fi
    log "podman system prune as $u:"
    if [[ $u == "$(id -un)" ]]; then
      podman system prune -af --volumes >&2 || return 1
    elif [[ $u == root ]]; then
      "${SUDO[@]}" podman system prune -af --volumes >&2 || return 1
    else
      # Another user's rootless store, reached from the root guard: runuser
      # gives a clean login context so podman resolves that user's XDG dirs
      # and storage config.
      runuser -u "$u" -- podman system prune -af --volumes >&2 || return 1
    fi
  done
}

# Stage 2: per-test debug logs. CI wipes this dir at job start anyway;
# entries older than a day are debris from crashed or cancelled runs.
stage_test_logs() {
  local dir=/tmp/fcvm-test-logs
  if [[ ! -d $dir ]]; then
    log "no $dir; nothing to do"
    return 0
  fi
  "${SUDO[@]}" find "$dir" -mindepth 1 -maxdepth 1 -mmin "+$((LOG_AGE_DAYS * 1440))" -print0 |
    delete_candidates "stale test logs (>${LOG_AGE_DAYS}d)"
}

# Stage 3: cargo/nextest build payloads. Each managed physical generation is
# considered independently; the cargo-target parent is only a namespace and
# must never be a candidate. Reclaim retires the old namespace before
# truncating bytes, so the next Cargo wrapper publishes a fresh generation.
stage_target_dirs() {
  local runner_root='' btrfs_target_root=''

  # One privileged process enumerates every root, atomically opens and locks
  # candidates as it discovers them, retains those fds until all enumeration
  # has succeeded, and only then validates/reclaims. A pathname or dev:ino passed
  # between two processes has an unavoidable replacement/ABA gap.
  if dir_exists "$RUNNER_WORK_ROOT"; then
    runner_root="$RUNNER_WORK_ROOT"
  fi
  if dir_exists "$BTRFS_ROOT/cargo-target"; then
    btrfs_target_root="$BTRFS_ROOT/cargo-target"
  fi
  if [[ -z $runner_root && -z $btrfs_target_root ]]; then
    log "no cargo target dirs found"
    return 0
  fi

  "${SUDO[@]}" env \
    -u FCVM_PRUNE_TEST_SYNC_SOCKET \
    -u FCVM_PRUNE_TEST_AFTER_RELEASE_SOCKET \
    -u FCVM_PRUNE_TEST_FAIL_AFTER_FINGERPRINTS \
    "$TARGET_PRUNE_HELPER" \
    "$TARGET_CUTOFF" "$DRY_RUN" "$runner_root" "$btrfs_target_root"
}

# Stage 4a helper: content-addressed image-cache entries
# (<digest>.docker.tar / <digest>.storage-v2.img). The age gate alone makes
# racing an in-progress build essentially impossible (a build in progress has
# a fresh file), but where a <digest>.lock exists we additionally delete under
# the same per-digest flock the image builders take — a held lock means an
# active build, so the entry is kept. Builders inside VMs bypass flock (it
# does not cross the FUSE boundary) but write unique temp names + atomic
# rename, so the worst case of racing one is a benign cache miss re-build.
prune_image_cache() {
  local ic="$BTRFS_ROOT/image-cache" f base lock rc size total=0 count=0 verb="freed"
  if ! dir_exists "$ic"; then
    return 0
  fi
  if ((DRY_RUN)); then
    verb="would be freed"
  fi
  while IFS= read -r -d '' f; do
    size="$("${SUDO[@]}" du -sb -- "$f" | awk '{print $1}')" || return 1
    if ((DRY_RUN)); then
      log "  would delete: $f ($(human "$size"))"
      total=$((total + size))
      count=$((count + 1))
      continue
    fi
    base="$(basename -- "$f")"
    lock="$ic/${base%%.*}.lock"
    if [[ -e $lock ]]; then
      if "${SUDO[@]}" flock -n -E 42 "$lock" rm -f -- "$f"; then
        log "  deleted: $f ($(human "$size"))"
      else
        rc=$?
        if ((rc == 42)); then
          log "  keeping (concurrent build holds $lock): $f"
          continue
        fi
        log "ERROR: deleting $f failed (rc=$rc)"
        return 1
      fi
    else
      "${SUDO[@]}" rm -f -- "$f" || return 1
      log "  deleted: $f ($(human "$size"))"
    fi
    total=$((total + size))
    count=$((count + 1))
  done < <("${SUDO[@]}" find "$ic" -maxdepth 1 -type f \
    \( -name '*.docker.tar' -o -name '*.storage-v2.img' \) \
    ! -newermt "$CACHE_CUTOFF" ! -newerat "$CACHE_CUTOFF" -print0)
  log "[image-cache entries (idle >${CACHE_AGE_DAYS}d)] $count entries, $(human "$total") $verb"
}

# Stage 4: shared content-addressed caches on the btrfs volume. Everything
# here is keyed by digest/SHA and regenerated on the next cache miss; only
# entries with no read/write activity for CACHE_AGE_DAYS are pruned, which
# excludes everything the current checkout's SHAs reference (see header).
stage_btrfs_caches() {
  if ! dir_exists "$BTRFS_ROOT"; then
    log "no $BTRFS_ROOT; nothing to do"
    return 0
  fi

  prune_image_cache || return 1

  # Snapshot dirs (default, root and container data dirs). Active snapshots
  # are being read by serve/restore processes and stay within the age gate.
  local sd d
  for sd in "$BTRFS_ROOT/snapshots" "$BTRFS_ROOT/root/snapshots" "$BTRFS_ROOT/container/snapshots"; do
    dir_exists "$sd" || continue
    while IFS= read -r -d '' d; do
      if recently_used "$d" "$CACHE_CUTOFF"; then
        log "  keeping (active within ${CACHE_AGE_DAYS}d): $d"
      else
        printf '%s\0' "$d"
      fi
    done < <("${SUDO[@]}" find "$sd" -mindepth 1 -maxdepth 1 -type d -print0) |
      delete_candidates "idle snapshots in $sd (>${CACHE_AGE_DAYS}d)" || return 1
  done

  # Content-addressed kernel/rootfs/initrd/firecracker assets. Idle-gated
  # only (no flock: an in-progress build's file is by definition fresh, so it
  # can never be a candidate). *.lock siblings are never deleted.
  for d in kernels rootfs initrd firecracker; do
    dir_exists "$BTRFS_ROOT/$d" || continue
    "${SUDO[@]}" find "$BTRFS_ROOT/$d" -maxdepth 1 -type f ! -name '*.lock' \
      ! -newermt "$CACHE_CUTOFF" ! -newerat "$CACHE_CUTOFF" -print0 |
      delete_candidates "idle $d assets (>${CACHE_AGE_DAYS}d)" || return 1
  done

  # Core dumps from crashed VMs/tests: pure debris after LOG_AGE_DAYS.
  if dir_exists "$BTRFS_ROOT/cores"; then
    "${SUDO[@]}" find "$BTRFS_ROOT/cores" -mindepth 1 -maxdepth 1 \
      -mmin "+$((LOG_AGE_DAYS * 1440))" -print0 |
      delete_candidates "core dumps (>${LOG_AGE_DAYS}d)" || return 1
  fi
}

# Stop (and disable, so a reboot cannot resurrect it) the GitHub Actions
# runner service. The unit name embeds the registered runner name
# (actions.runner.<org-repo>.<runner-name>.service), so the quarantine log
# names the runner that needs attention.
quarantine_runner() {
  local units=() u
  mapfile -t units < <(systemctl list-units --all --plain --no-legend 'actions.runner.*.service' | awk '{print $1}')
  if ((${#units[@]} == 0)); then
    log "quarantine: no actions.runner service unit on this host"
    return 0
  fi
  for u in "${units[@]}"; do
    log "QUARANTINE: stopping and disabling $u — this runner is out of disk and now offline; the autoscaler should replace it"
    "${SUDO[@]}" systemctl stop "$u" || return 1
    "${SUDO[@]}" systemctl disable "$u" || return 1
  done
  "${SUDO[@]}" touch /run/runner-disk-guard-quarantined
}

main() {
  if ((TARGET_DIRS_ONLY)); then
    stage_target_dirs
    return
  fi
  local runner_id="${RUNNER_NAME:-$(hostname)}"
  log "runner: $runner_id"
  log "monitored filesystems: ${MON[*]}"
  log "df before:"
  df_report

  if ((DRY_RUN)); then
    log "DRY_RUN=1: auditing all cleanup stages regardless of thresholds; nothing will be deleted"
  elif ! any_below "$MIN_FREE_GB"; then
    log "OK: every monitored filesystem has >= ${MIN_FREE_GB}GiB free; no cleanup needed"
    return 0
  fi

  local stages=(stage_podman_prune stage_test_logs stage_target_dirs stage_btrfs_caches)
  local failed=() s before after
  for s in "${stages[@]}"; do
    if ((!DRY_RUN)) && ! any_below "$MIN_FREE_GB"; then
      log "recovered above ${MIN_FREE_GB}GiB free; skipping remaining stages"
      break
    fi
    log "=== $s ==="
    before="$(total_avail)"
    if ! "$s"; then
      failed+=("$s")
      log "ERROR: $s failed; continuing with remaining stages to recover space, will exit nonzero"
    fi
    after="$(total_avail)"
    # btrfs frees extents asynchronously (cleaner thread), so a stage's bytes
    # can show up in a later stage's delta — trust the per-entry sizes above
    # and the overall before/after df, not a single stage's number.
    log "=== $s freed $(human "$((after > before ? after - before : 0))") (df delta across monitored filesystems) ==="
  done

  log "df after:"
  df_report

  if ((!DRY_RUN)) && any_below "$HARD_FLOOR_GB"; then
    log "FATAL: runner '$runner_id' still below ${HARD_FLOOR_GB}GiB free after cleanup"
    # GitHub error annotation (parsed from stdout) so the failure names the
    # runner at the top of the job page instead of a mystery ENOSPC later.
    echo "::error title=Runner disk full::Runner '$runner_id' is below ${HARD_FLOOR_GB}GiB free after cleanup - replace or manually clean this runner"
    if ((QUARANTINE)); then
      quarantine_runner || log "ERROR: quarantine failed; runner may still accept jobs"
    fi
    return 1
  fi

  if ((${#failed[@]} > 0)); then
    log "ERROR: cleanup stage(s) failed: ${failed[*]}"
    return 1
  fi
  log "done"
}

main "$@"
