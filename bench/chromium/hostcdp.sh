#!/bin/bash
# Host-native direct-CDP warm-pool baseline: the same bench image as a podman
# container ON THE HOST (no VM), driven by the same cdpdrive.py at Chromium's
# own CDP port, same page, same rep discipline as reqbench's cdp arm.
#
# This exists because the corrected record's 218 ms host warm-pool figure was
# measured on the EXEC-style driver: comparing reqbench's direct-CDP VM arm
# against it inflates fcvm's ratio in fcvm's favor (the host side carries
# driver-startup overhead the VM side already shed). The shared-nothing
# premium must be (VM direct-CDP) / (host direct-CDP), same driver both sides.
set -euo pipefail

case "$#" in
    0) HOSTCDP_PROCESS_ROLE=bootstrap ;;
    1)
        [ "$1" = --lifecycle-worker ] \
            || { echo "REFUSING: unknown argument '$1'" >&2; exit 2; }
        HOSTCDP_PROCESS_ROLE=worker
        ;;
    *) echo "REFUSING: hostcdp accepts no user arguments" >&2; exit 2 ;;
esac

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
IMAGE="${IMAGE:-localhost/chromium-bench-req}"
IMAGE_ID="${IMAGE_ID:-}"
# REPS is the MEASURED rep count and WARMUP is EXTRA, exactly as reqbench.py
# reads --reps/--warmup ("for rep in range(args.warmup + args.reps)"), so the
# campaign's REPS/WARMUP can be handed to both arms and produce the same
# schedule. The default measured count is what REPS=202 WARMUP=2 used to yield.
REPS="${REPS:-200}"
WARMUP="${WARMUP:-2}"
URL="${URL:-http://127.0.0.1:8000/medium.html}"
CDP_PORT="${CDP_PORT:-9222}"
RUNID="${RUNID:-$(tr -d - </proc/sys/kernel/random/uuid)}"
RESULTS="${RESULTS:-$HERE/results/hostcdp-$RUNID}"
CNAME="hostcdp-$RUNID"
LOADAVG_FILE="${LOADAVG_FILE:-/proc/loadavg}"
PODMAN_CREATE_TIMEOUT_SECS="${PODMAN_CREATE_TIMEOUT_SECS:-120}"
PODMAN_CREATE_KILL_AFTER_SECS="${PODMAN_CREATE_KILL_AFTER_SECS:-5}"
PODMAN_CREATE_QUIESCE_TIMEOUT_SECS="${PODMAN_CREATE_QUIESCE_TIMEOUT_SECS:-30}"
CONTAINER_CREATE_OPS_DIR="${CONTAINER_CREATE_OPS_DIR:-}"
CONTAINER_OWNER_TOKEN="${CONTAINER_OWNER_TOKEN:-$(tr -d - </proc/sys/kernel/random/uuid)}"
readonly CONTAINER_OWNER_TOKEN
OWNER_LABEL_KEY="io.fcvm.bench.owner"
readonly OWNER_LABEL_KEY
COMPARISON_LABEL="${COMPARISON_LABEL:-}"
CPU_BUDGET="${CPU_BUDGET:-}"
SOURCE_REVISION="${SOURCE_REVISION:-}"
REQBENCH_RUNTIME_MANIFEST="${REQBENCH_RUNTIME_MANIFEST:-}"
REQBENCH_RUNTIME_BUNDLE_SHA256="${REQBENCH_RUNTIME_BUNDLE_SHA256:-}"
CORPUS_EXTRA_RUNTIME_MANIFEST="${CORPUS_EXTRA_RUNTIME_MANIFEST:-}"
CORPUS_EXTRA_RUNTIME_BUNDLE_SHA256="${CORPUS_EXTRA_RUNTIME_BUNDLE_SHA256:-}"
CONTAINER_ID=
CONTAINER_OWNERSHIP_VERIFIED=false
CREATE_OUTPUT_PATH=
CREATE_OP_LOCK_FD=
CREATE_OP_STARTED=false
CREATE_OP_QUIESCENT=true
CREATE_OUTCOME_CHECKED=false
CONTAINER_REMOVED=false

[[ "$RUNID" =~ ^[A-Za-z0-9][A-Za-z0-9_.-]{0,95}$ ]] \
    || { echo "REFUSING: RUNID must be 1-96 filename-safe characters" >&2; exit 2; }
[[ "$CONTAINER_OWNER_TOKEN" =~ ^[0-9a-f]{32}$ ]] \
    || { echo "REFUSING: CONTAINER_OWNER_TOKEN must be exactly 32 lowercase hex characters" >&2; exit 2; }
[[ "$COMPARISON_LABEL" =~ ^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$ ]] \
    || { echo "REFUSING: COMPARISON_LABEL must be 1-64 filename-safe characters" >&2; exit 2; }
[[ "$REPS" =~ ^[0-9]+$ && "$WARMUP" =~ ^[0-9]+$ ]] \
    || { echo "REFUSING: REPS and WARMUP must be nonnegative integers" >&2; exit 2; }
if ! [[ "$PODMAN_CREATE_TIMEOUT_SECS" =~ ^[0-9]+$ ]] \
        || [ "$((10#$PODMAN_CREATE_TIMEOUT_SECS))" -lt 1 ]; then
    echo "REFUSING: PODMAN_CREATE_TIMEOUT_SECS must be a positive integer" >&2
    exit 2
fi
PODMAN_CREATE_TIMEOUT_SECS=$((10#$PODMAN_CREATE_TIMEOUT_SECS))
for value_name in PODMAN_CREATE_KILL_AFTER_SECS PODMAN_CREATE_QUIESCE_TIMEOUT_SECS; do
    value=${!value_name}
    if ! [[ "$value" =~ ^[0-9]+$ ]] || [ "$((10#$value))" -lt 1 ]; then
        echo "REFUSING: $value_name must be a positive integer" >&2
        exit 2
    fi
    printf -v "$value_name" '%d' "$((10#$value))"
done
if ! [[ "$CDP_PORT" =~ ^[0-9]+$ ]] \
        || [ "$CDP_PORT" -lt 1 ] || [ "$CDP_PORT" -gt 65535 ]; then
    echo "REFUSING: CDP_PORT must be in 1..65535" >&2
    exit 2
fi
for tool in awk cut date dirname flock git mkdir mktemp pgrep podman python3 rm sed \
        seq sha256sum sleep timeout tr uname; do
    command -v "$tool" >/dev/null 2>&1 \
        || { echo "REFUSING: '$tool' missing" >&2; exit 2; }
done
case "$CPU_BUDGET" in
    unlimited)
        [ -z "${CPUS:-}" ] \
            || { echo "REFUSING: CPU_BUDGET=unlimited requires CPUS to be unset" >&2; exit 2; }
        ;;
    vm-matched)
        [[ "${CPUS:-}" =~ ^([0-9]+([.][0-9]*)?|[.][0-9]+)$ ]] \
            || { echo "REFUSING: CPU_BUDGET=vm-matched requires a positive finite CPUS number" >&2; exit 2; }
        awk -v value="$CPUS" 'BEGIN { exit !(value > 0) }' \
            || { echo "REFUSING: CPU_BUDGET=vm-matched requires CPUS > 0" >&2; exit 2; }
        ;;
    *) echo "REFUSING: CPU_BUDGET must be unlimited or vm-matched" >&2; exit 2 ;;
esac

log() { printf '%s %s\n' "$(date +%H:%M:%S)" "$*" >&2; }

# The worker only writes a hidden pending summary. The bootstrap calls this
# after the detached supervisor has drained every descendant and its mandatory
# finalizer has returned, so neither artifact can authorize a run whose outer
# lifecycle later fails.
publish_complete() {
    python3 - "$RESULTS/complete.json" "$RUNID" \
            "$RESULTS/run.json" "$RESULTS/hostcdp.jsonl" \
            "$RESULTS/.summary.pending" "$RESULTS/summary.json" \
            "$RESULTS/.summary.lock" "$RESULTS/WITHDRAWN" <<'PY'
import fcntl
import hashlib
import json
import os
import stat
import sys
import tempfile

(output_path, run_id, run_path, rows_path, pending_summary_path,
 summary_path, lock_path, withdrawn_path) = sys.argv[1:]
directory = os.path.dirname(output_path)


def identity(path):
    digest = hashlib.sha256()
    size = 0
    with open(path, "rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            size += len(chunk)
            digest.update(chunk)
    return {"size": size, "sha256": digest.hexdigest()}


def write_withdrawn(reason):
    fd, temporary = tempfile.mkstemp(prefix=".withdrawn.", dir=directory)
    try:
        with os.fdopen(fd, "w") as target:
            target.write(reason.replace("\n", " ") + "\n")
            target.flush()
            os.fsync(target.fileno())
        os.replace(temporary, withdrawn_path)
    except BaseException:
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass
        raise


lock_fd = os.open(lock_path, os.O_RDWR | os.O_NOFOLLOW)
temporary = None
try:
    lock_stat = os.fstat(lock_fd)
    if not stat.S_ISREG(lock_stat.st_mode) or lock_stat.st_nlink != 1:
        raise RuntimeError("permanent summary lock is not one regular file")
    fcntl.flock(lock_fd, fcntl.LOCK_EX)
    if os.path.exists(withdrawn_path):
        raise RuntimeError("worker withdrew the run before publication")
    if os.path.lexists(output_path) or os.path.lexists(summary_path):
        raise RuntimeError("publication target already exists")
    with open(pending_summary_path, "r") as source:
        pending_summary = json.load(source)
    if not isinstance(pending_summary, dict):
        raise RuntimeError("pending summary is not a JSON object")

    record = {
        "schema_version": 1,
        "run_id": run_id,
        "artifacts": {
            "run.json": identity(run_path),
            "hostcdp.jsonl": identity(rows_path),
        },
    }
    os.replace(pending_summary_path, summary_path)
    fd, temporary = tempfile.mkstemp(prefix=".complete.", dir=directory)
    with os.fdopen(fd, "w") as target:
        json.dump(record, target, sort_keys=True)
        target.write("\n")
        target.flush()
        os.fsync(target.fileno())
    os.replace(temporary, output_path)
    temporary = None
    if os.environ.get("HOSTCDP_COMPLETE_FAIL_AFTER_RENAME") == "1":
        raise OSError("injected failure after completion rename")
    directory_fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(directory_fd)
    finally:
        os.close(directory_fd)
except BaseException as error:
    if temporary is not None:
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass
    invalidation_error = None
    for path in (output_path, summary_path, pending_summary_path):
        try:
            os.unlink(path)
        except FileNotFoundError:
            pass
        except OSError as exc:
            invalidation_error = invalidation_error or exc
    try:
        write_withdrawn(
            f"hostcdp publication failed: {type(error).__name__}: {error}"
        )
        directory_fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
    except OSError as exc:
        invalidation_error = invalidation_error or exc
    if invalidation_error is not None:
        raise RuntimeError(
            f"publication failed and invalidation was incomplete: {invalidation_error}"
        ) from error
    raise
finally:
    os.close(lock_fd)
PY
}

withdraw_guarded_run() {
    local reason=$1
    [ -d "$RESULTS" ] || return 0
    python3 - "$RESULTS" "$reason" <<'PY'
import fcntl
import os
import stat
import sys
import tempfile

directory, reason = sys.argv[1:]
lock_path = os.path.join(directory, ".summary.lock")
lock_fd = os.open(lock_path, os.O_RDWR | os.O_NOFOLLOW)
try:
    lock_stat = os.fstat(lock_fd)
    if not stat.S_ISREG(lock_stat.st_mode) or lock_stat.st_nlink != 1:
        raise RuntimeError("permanent summary lock is not one regular file")
    fcntl.flock(lock_fd, fcntl.LOCK_EX)
    for name in ("complete.json", "summary.json", ".summary.pending"):
        try:
            os.unlink(os.path.join(directory, name))
        except FileNotFoundError:
            pass

    output_path = os.path.join(directory, "WITHDRAWN")
    fd, temporary = tempfile.mkstemp(prefix=".withdrawn.", dir=directory)
    try:
        with os.fdopen(fd, "w") as target:
            target.write(reason.replace("\n", " ") + "\n")
            target.flush()
            os.fsync(target.fileno())
        os.replace(temporary, output_path)
        directory_fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
    except BaseException:
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass
        raise
finally:
    os.close(lock_fd)
PY
}

if [ "$HOSTCDP_PROCESS_ROLE" = bootstrap ]; then
    export RUNID RESULTS CONTAINER_OWNER_TOKEN
    create_lock_dir="${CONTAINER_CREATE_OPS_DIR:-$RESULTS/container-create-ops}"
    create_lock_path="$create_lock_dir/hostcdp-$RUNID-$CONTAINER_OWNER_TOKEN.lock"
    guardian_parent=$BASHPID
    FCVM_FINALIZER_MODE=container \
            FCVM_CONTAINER_NAME="$CNAME" \
            FCVM_CONTAINER_OWNER_TOKEN="$CONTAINER_OWNER_TOKEN" \
            FCVM_CONTAINER_CREATE_LOCK_PATH="$create_lock_path" \
            python3 "$HERE/phase_supervisor.py" \
                --detach --expected-parent "$guardian_parent" \
                --finalizer "$HERE/host_resource_finalizer.py" \
                --finalizer-timeout 180 -- \
                bash "$HERE/hostcdp.sh" --lifecycle-worker &
    guardian_pid=$!
    trap 'exit 130' INT
    trap 'exit 143' TERM
    set +e
    wait "$guardian_pid"
    guardian_rc=$?
    set -e
    trap - INT TERM
    if [ "$guardian_rc" -eq 0 ]; then
        if publish_complete; then
            exit 0
        else
            guardian_rc=$?
        fi
    elif ! withdraw_guarded_run \
            "hostcdp outer lifecycle exited $guardian_rc; raw completion is not authorized"; then
        log "FAILED: outer lifecycle withdrawal was incomplete"
    fi
    exit "$guardian_rc"
fi

REPO=$(git -C "$HERE" rev-parse --show-toplevel 2>/dev/null) || REPO=""

compute_harness_sha256() {
    python3 - "$HERE" <<'PY'
import hashlib
import os
import sys

here = sys.argv[1]
names = ("reqbench.py", "cdpdrive.py", "render.py", "wddrive.py", "reqbench.sh")
h = hashlib.sha256()
h.update(b"fcvm-chromium-request-harness-v1\0")
for name in names:
    encoded = name.encode()
    h.update(len(encoded).to_bytes(4, "big"))
    h.update(encoded)
    with open(os.path.join(here, name), "rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            h.update(chunk)
print(h.hexdigest())
PY
}

verify_runtime_manifest() {
    local manifest=$1 expected=$2 label=$3 actual directory name
    [ -f "$manifest" ] \
        || { log "REFUSING: $label runtime manifest is missing: $manifest"; return 2; }
    actual=$(sha256sum "$manifest" | cut -d' ' -f1) \
        || { log "REFUSING: cannot hash $label runtime manifest"; return 2; }
    [[ "$expected" =~ ^[0-9a-f]{64}$ ]] \
        || { log "REFUSING: $label runtime identity is not a sha256"; return 2; }
    [ "$actual" = "$expected" ] \
        || { log "REFUSING: $label runtime manifest identity changed"; return 2; }
    directory=$(dirname -- "$manifest")
    name=${manifest##*/}
    (cd "$directory" && sha256sum --check --strict --status "$name") \
        || { log "REFUSING: $label runtime bytes changed"; return 2; }
}

canonicalize_image_id() {
    local raw=$1 digest
    if [[ "$raw" =~ ^sha256:([0-9a-f]{64})$ ]]; then
        digest=${BASH_REMATCH[1]}
    elif [[ "$raw" =~ ^([0-9a-f]{64})$ ]]; then
        digest=${BASH_REMATCH[1]}
    else
        return 2
    fi
    printf 'sha256:%s\n' "$digest"
}

resolve_image_id() {
    local reference=$1 raw resolved
    raw=$(timeout --kill-after=5s 30s podman image inspect --format '{{.Id}}' "$reference") \
        || { log "REFUSING: cannot resolve image identity for $reference"; return 2; }
    resolved=$(canonicalize_image_id "$raw") \
        || { log "REFUSING: image $reference resolved to an invalid identity"; return 2; }
    printf '%s\n' "$resolved"
}

verify_container_image() {
    local reference=$1 raw actual
    raw=$(timeout --kill-after=5s 30s podman inspect --type container --format '{{.Image}}' "$reference") \
        || { log "REFUSING: cannot inspect image identity for container $reference"; return 2; }
    actual=$(canonicalize_image_id "$raw") \
        || { log "REFUSING: container $reference reports an invalid image identity"; return 2; }
    [ "$actual" = "$runtime_image_id" ] \
        || { log "REFUSING: container $reference uses image $actual, expected $runtime_image_id"; return 2; }
}

compute_live_reqbench_bundle_sha256() {
    python3 - "$HERE" "$REPO/target/release/fcvm" "$REPO/target/release/fc-agent" <<'PY'
import hashlib
import os
import sys

here, fcvm, fc_agent = sys.argv[1:]
inputs = (
    ("fcvm", fcvm),
    ("fc-agent", fc_agent),
    ("reqbench.sh", os.path.join(here, "reqbench.sh")),
    ("reqbench.py", os.path.join(here, "reqbench.py")),
    ("reqanalyze.py", os.path.join(here, "reqanalyze.py")),
    ("cdpdrive.py", os.path.join(here, "cdpdrive.py")),
    ("render.py", os.path.join(here, "render.py")),
    ("wddrive.py", os.path.join(here, "wddrive.py")),
)
manifest = bytearray()
for name, path in inputs:
    digest = hashlib.sha256()
    with open(path, "rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    manifest.extend(f"{digest.hexdigest()}  {name}\n".encode())
print(hashlib.sha256(manifest).hexdigest())
PY
}

if [ -n "$SOURCE_REVISION" ]; then
    [[ "$SOURCE_REVISION" =~ ^[0-9a-f]{40}([0-9a-f]{24})?$ ]] \
        || { log "REFUSING: SOURCE_REVISION is not a 40- or 64-character commit ID"; exit 2; }
    source_revision="$SOURCE_REVISION"
else
    [ -n "$REPO" ] || { log "REFUSING: $HERE is not in a git worktree"; exit 2; }
    source_revision=$(git -C "$REPO" rev-parse HEAD) \
        || { log "REFUSING: cannot read source revision"; exit 2; }
fi
readonly source_revision

if [ -n "$REQBENCH_RUNTIME_MANIFEST" ]; then
    verify_runtime_manifest "$REQBENCH_RUNTIME_MANIFEST" \
        "$REQBENCH_RUNTIME_BUNDLE_SHA256" reqbench || exit 2
    runtime_bundle_sha256="$REQBENCH_RUNTIME_BUNDLE_SHA256"
else
    [ -n "$REPO" ] \
        || { log "REFUSING: a staged host run must name REQBENCH_RUNTIME_MANIFEST"; exit 2; }
    runtime_bundle_sha256=$(compute_live_reqbench_bundle_sha256) \
        || { log "REFUSING: cannot hash the live reqbench runtime"; exit 2; }
fi
[[ "$runtime_bundle_sha256" =~ ^[0-9a-f]{64}$ ]] \
    || { log "REFUSING: reqbench runtime seal produced an invalid digest"; exit 2; }

corpus_extra_runtime_bundle_sha256=""
if [ -n "$CORPUS_EXTRA_RUNTIME_MANIFEST" ] \
        || [ -n "$CORPUS_EXTRA_RUNTIME_BUNDLE_SHA256" ]; then
    verify_runtime_manifest "$CORPUS_EXTRA_RUNTIME_MANIFEST" \
        "$CORPUS_EXTRA_RUNTIME_BUNDLE_SHA256" corpus-extra || exit 2
    corpus_extra_runtime_bundle_sha256="$CORPUS_EXTRA_RUNTIME_BUNDLE_SHA256"
fi
harness_sha256=$(compute_harness_sha256) \
    || { log "REFUSING: cannot hash request harness sources"; exit 2; }
hostcdp_sha256=$(sha256sum "$HERE/hostcdp.sh" | cut -d' ' -f1) \
    || { log "REFUSING: cannot hash hostcdp.sh"; exit 2; }
phase_supervisor_sha256=$(sha256sum "$HERE/phase_supervisor.py" | cut -d' ' -f1) \
    || { log "REFUSING: cannot hash phase_supervisor.py"; exit 2; }
host_resource_finalizer_sha256=$(sha256sum "$HERE/host_resource_finalizer.py" | cut -d' ' -f1) \
    || { log "REFUSING: cannot hash host_resource_finalizer.py"; exit 2; }
[[ "$harness_sha256" =~ ^[0-9a-f]{64}$ \
        && "$hostcdp_sha256" =~ ^[0-9a-f]{64}$ \
        && "$phase_supervisor_sha256" =~ ^[0-9a-f]{64}$ \
        && "$host_resource_finalizer_sha256" =~ ^[0-9a-f]{64}$ ]] \
    || { log "REFUSING: source seal produced an invalid digest"; exit 2; }
read -r host_boot_id < /proc/sys/kernel/random/boot_id \
    || { log "REFUSING: cannot read host boot identity"; exit 2; }
host_machine=$(uname -m) || { log "REFUSING: cannot read host machine"; exit 2; }
host_kernel=$(uname -r) || { log "REFUSING: cannot read host kernel"; exit 2; }
if [ -z "$host_boot_id" ] || [ -z "$host_machine" ] || [ -z "$host_kernel" ]; then
    log "REFUSING: host identity is incomplete"
    exit 2
fi

results_parent=$(dirname -- "$RESULTS")
mkdir -p -- "$results_parent"
if ! mkdir -- "$RESULTS" 2>/dev/null; then
    log "REFUSING: RESULTS must name a new directory; could not claim $RESULTS"
    exit 2
fi
exec {SUMMARY_LOCK_FD}>"$RESULTS/.summary.lock" \
    || { log "REFUSING: cannot open permanent summary lock"; exit 2; }
flock -x "$SUMMARY_LOCK_FD" \
    || { log "REFUSING: cannot acquire permanent summary lock"; exit 2; }
if [ -z "$CONTAINER_CREATE_OPS_DIR" ]; then
    CONTAINER_CREATE_OPS_DIR="$RESULTS/container-create-ops"
    mkdir -- "$CONTAINER_CREATE_OPS_DIR" \
        || { log "REFUSING: cannot create container operation directory"; exit 2; }
elif [ ! -d "$CONTAINER_CREATE_OPS_DIR" ]; then
    log "REFUSING: CONTAINER_CREATE_OPS_DIR is not an existing directory: $CONTAINER_CREATE_OPS_DIR"
    exit 2
fi
CREATE_OP_LOCK_PATH="$CONTAINER_CREATE_OPS_DIR/hostcdp-$RUNID-$CONTAINER_OWNER_TOKEN.lock"
readonly CONTAINER_CREATE_OPS_DIR CREATE_OP_LOCK_PATH

container_owns_cdp() {
    timeout --kill-after=5s 30s podman exec "$CONTAINER_ID" python3 -c '
import os
import sys

port = int(sys.argv[1])
inodes = set()
for table in ("/proc/net/tcp", "/proc/net/tcp6"):
    try:
        rows = open(table).read().splitlines()[1:]
    except OSError:
        continue
    for row in rows:
        fields = row.split()
        if len(fields) <= 9 or fields[3] != "0A":
            continue
        try:
            local_port = int(fields[1].rsplit(":", 1)[1], 16)
        except (IndexError, ValueError):
            continue
        if local_port == port:
            inodes.add(fields[9])
if not inodes:
    raise SystemExit(1)
for pid in os.listdir("/proc"):
    if not pid.isdigit():
        continue
    try:
        fds = os.listdir(f"/proc/{pid}/fd")
    except OSError:
        continue
    for fd in fds:
        try:
            target = os.readlink(f"/proc/{pid}/fd/{fd}")
        except OSError:
            continue
        if target.startswith("socket:[") and target[8:-1] in inodes:
            raise SystemExit(0)
raise SystemExit(1)
' "$CDP_PORT" >/dev/null 2>&1
}

record_owned_container_id() {
    local candidate=$1
    [[ "$candidate" =~ ^[0-9a-f]{64}$ ]] || return 2
    if [ -n "$CONTAINER_ID" ] && [ "$CONTAINER_ID" != "$candidate" ]; then
        log "REFUSING: container ownership changed from $CONTAINER_ID to $candidate"
        return 2
    fi
    CONTAINER_ID=$candidate
}

inspect_owned_container() {
    local reference=$1 line inspect_status exists_status inspected_id inspected_token extra
    if line=$(timeout --kill-after=5s 30s podman inspect --type container \
            --format '{{.Id}}|{{index .Config.Labels "io.fcvm.bench.owner"}}' \
            "$reference" 2>/dev/null); then
        :
    else
        inspect_status=$?
        if timeout --kill-after=5s 30s podman container exists "$reference" >/dev/null 2>&1; then
            log "REFUSING: container $reference exists but its identity could not be inspected (inspect status=$inspect_status)"
            return 3
        else
            exists_status=$?
        fi
        [ "$exists_status" -eq 1 ] && return 1
        log "REFUSING: cannot establish whether container $reference exists (inspect status=$inspect_status exists status=$exists_status)"
        return 3
    fi
    IFS='|' read -r inspected_id inspected_token extra <<<"$line"
    [ -z "$extra" ] || return 2
    [ "$inspected_token" = "$CONTAINER_OWNER_TOKEN" ] || return 2
    record_owned_container_id "$inspected_id" || return 2
    CONTAINER_OWNERSHIP_VERIFIED=true
}

# Container creation is run to a definitive CLI completion while holding the
# create-operation lock. Once it returns, one owner-token inspection is an
# absence proof; there is no killed client left that can commit later.
adopt_completed_create() {
    inspect_owned_container "${CONTAINER_ID:-$CNAME}"
}

# URL may name ONE url (today's contract) or a comma-separated list. The list is
# cycled exactly as reqbench.py cycles the VM arm's schedule -- url_for_rep()
# returns urls[rep % len(urls)], rep counted from 0 across warmup and measured
# reps alike -- so this host control can run the SAME corpus schedule as the VM
# arm rather than a different workload. Whitespace around a member is stripped
# and empty members are dropped, matching reqbench.parse_urls.
mapfile -t URLS < <(printf '%s' "$URL" | tr ',' '\n' | sed 's/^[[:space:]]*//; s/[[:space:]]*$//' | awk 'NF')
[ "${#URLS[@]}" -gt 0 ] || { log "REFUSING: URL names no urls"; exit 2; }
# Nothing to summarise, and the summary would die on an empty list after the
# warmup reps had already run.
[ "$REPS" -ge 1 ] || { log "REFUSING: REPS must be >= 1 (it is the MEASURED count; WARMUP is extra), got $REPS"; exit 2; }
# reqbench refuses a multi-URL run with fewer than two full cycles of warmup;
# the same floor here keeps the two schedules the same shape rather than
# comparing a warmed VM arm against a cold host arm.
if [ "${#URLS[@]}" -gt 1 ] && [ "$WARMUP" -lt $((2 * ${#URLS[@]})) ]; then
    log "REFUSING: a ${#URLS[@]}-url schedule needs WARMUP >= $((2 * ${#URLS[@]})) (two full cycles), got $WARMUP"
    exit 2
fi

# Same quiet-box refusal as the VM harness: a contaminated baseline poisons
# every ratio computed against it. SETTLE_WAIT_SECS > 0 bounds a wait for the
# box to go quiet before refusing (same knob as reqbench.sh guard_quiet; the
# Makefile runs this right after `build`, whose load a 1-minute average still
# carries). Default 0 keeps the fail-fast refusal.
SETTLE_WAIT_SECS="${SETTLE_WAIT_SECS:-0}"
[[ "$SETTLE_WAIT_SECS" =~ ^[0-9]+$ ]] \
    || { log "SETTLE_WAIT_SECS must be a whole number of seconds (got '$SETTLE_WAIT_SECS')"; exit 2; }
quiet_sample() {
    if la=$(cut -d' ' -f1 "$LOADAVG_FILE" 2>&1); then
        la_status=0
    else
        la_status=$?
    fi
    # This function runs as an `until` condition, where set -e is suppressed
    # and awk reads an empty string as 0, so a missing, unreadable, or empty
    # LOADAVG_FILE would otherwise pass the gate without a load reading.
    if [ "$la_status" -ne 0 ] || ! [[ "$la" =~ ^[0-9]+([.][0-9]+)?$ ]]; then
        log "REFUSING: no numeric 1-minute load readable from $LOADAVG_FILE (status=$la_status got '$la')"
        exit 2
    fi
    fc=0
    for process in fcvm firecracker; do
        if count=$(pgrep -c -x "$process" 2>&1); then rc=0; else rc=$?; fi
        case "$rc" in
            0)
                [[ "$count" =~ ^[0-9]+$ ]] \
                    || { log "REFUSING: pgrep returned a nonnumeric count for $process: $count"; exit 2; }
                fc=$((fc + count))
                ;;
            1) ;;
            *) log "REFUSING: pgrep exited $rc while checking $process: $count"; exit 2 ;;
        esac
    done
    [ "${ALLOW_BUSY:-0}" != 1 ] || return 0
    [ "${fc:-0}" -eq 0 ] || return 1
    # Same 1.0 gate as faultbench; the earlier printf-rounding form refused at 0.70.
    if awk -v l="$la" 'BEGIN{exit !(l >= 1.0)}'; then return 1; fi
    return 0
}
# Base 10: "08" passes the validator and is invalid octal to bash arithmetic.
SETTLE_WAIT_SECS=$((10#$SETTLE_WAIT_SECS))
deadline=$((SECONDS + SETTLE_WAIT_SECS))
until quiet_sample; do
    if [ "$SECONDS" -ge "$deadline" ]; then
        log "REFUSING: load=$la firecracker=$fc. ALLOW_BUSY=1 overrides and taints the run."
        exit 3
    fi
    nap=$((deadline - SECONDS))
    [ "$nap" -le 5 ] || nap=5
    log "settling: load=$la firecracker=$fc; re-sampling in ${nap}s ($((deadline - SECONDS))s left in the ${SETTLE_WAIT_SECS}s window)"
    sleep "$nap"
done

withdraw_failed_run() {
    local reason=$1
    (
        local withdrawal_lock_fd invalidation_failed=false
        if ! exec {withdrawal_lock_fd}<"$RESULTS"; then
            log "FAILED: could not open withdrawal lock for refused run"
            exit 1
        fi
        if ! flock -x "$withdrawal_lock_fd"; then
            log "FAILED: could not lock refused run for withdrawal"
            exit 1
        fi
        if ! rm -f -- "$RESULTS/complete.json" "$RESULTS/summary.json" \
                "$RESULTS/.summary.pending"; then
            log "FAILED: could not remove derived authorization from refused run"
            invalidation_failed=true
        fi
        if ! python3 - "$RESULTS/WITHDRAWN" "$reason" <<'PY'
import os
import sys
import tempfile

output_path, reason = sys.argv[1:]
directory = os.path.dirname(output_path)
fd, temporary = tempfile.mkstemp(prefix=".withdrawn.", dir=directory)
try:
    with os.fdopen(fd, "w") as target:
        target.write(reason.replace("\n", " ") + "\n")
        target.flush()
        os.fsync(target.fileno())
    os.replace(temporary, output_path)
    directory_fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(directory_fd)
    finally:
        os.close(directory_fd)
except BaseException:
    try:
        os.unlink(temporary)
    except FileNotFoundError:
        pass
    raise
PY
        then
            log "FAILED: could not mark refused run WITHDRAWN"
            exit 1
        fi
        [ "$invalidation_failed" = false ] || exit 1
    )
}

remove_owned_container() {
    local exists_status
    [ -n "$CONTAINER_ID" ] || return 0
    [ "$CONTAINER_OWNERSHIP_VERIFIED" = true ] || return 0
    [ "$CONTAINER_REMOVED" != true ] || return 0
    if ! timeout --kill-after=5s 30s podman rm -f -- "$CONTAINER_ID" >/dev/null 2>&1; then
        log "FAILED: could not remove owned container $CONTAINER_ID"
        return 1
    fi
    if timeout --kill-after=5s 30s podman container exists "$CONTAINER_ID" >/dev/null 2>&1; then
        log "FAILED: owned container $CONTAINER_ID survived podman rm"
        return 1
    else
        exists_status=$?
    fi
    if [ "$exists_status" -ne 1 ]; then
        log "FAILED: could not verify removal of owned container $CONTAINER_ID (exists status=$exists_status)"
        return 1
    fi
    CONTAINER_REMOVED=true
}

quiesce_create_operation() {
    [ "$CREATE_OP_STARTED" = true ] || return 0
    [ "$CREATE_OP_QUIESCENT" != true ] || return 0
    if [ -n "$CREATE_OP_LOCK_FD" ]; then
        if ! exec {CREATE_OP_LOCK_FD}>&-; then
            log "FAILED: cannot close container create-operation lease"
            return 1
        fi
        CREATE_OP_LOCK_FD=
    fi
    if ! exec {CREATE_OP_LOCK_FD}>"$CREATE_OP_LOCK_PATH"; then
        log "FAILED: cannot reopen container create-operation lock"
        CREATE_OP_LOCK_FD=
        return 2
    fi
    if ! flock -x -w "$PODMAN_CREATE_QUIESCE_TIMEOUT_SECS" \
            "$CREATE_OP_LOCK_FD"; then
        log "FAILED: container create operation did not quiesce within ${PODMAN_CREATE_QUIESCE_TIMEOUT_SECS}s"
        exec {CREATE_OP_LOCK_FD}>&-
        CREATE_OP_LOCK_FD=
        return 124
    fi
    CREATE_OP_QUIESCENT=true
}

# An empty lease means abnormal-exit cleanup must inspect Podman. Mark it
# retired only after the create tree is drained and the exact ID is absent, so
# a successful worker has no fallible container operation after publication.
retire_create_operation() {
    local retirement_fd marker_byte="" read_rc
    if [ "$CREATE_OP_STARTED" != true ] \
            || [ "$CREATE_OP_QUIESCENT" != true ] \
            || [ "$CONTAINER_REMOVED" != true ]; then
        log "FAILED: cannot retire a live or unproved container create operation"
        return 1
    fi
    if ! exec {retirement_fd}<>"$CREATE_OP_LOCK_PATH"; then
        log "FAILED: cannot open container create-operation lease for retirement"
        return 1
    fi
    if ! flock -x -w "$PODMAN_CREATE_QUIESCE_TIMEOUT_SECS" \
            "$retirement_fd"; then
        log "FAILED: cannot lock container create-operation lease for retirement"
        exec {retirement_fd}>&-
        return 1
    fi
    if IFS= read -r -n 1 marker_byte <&"$retirement_fd"; then
        log "FAILED: container create-operation lease was not empty before retirement"
        exec {retirement_fd}>&-
        return 1
    else
        read_rc=$?
    fi
    if [ "$read_rc" -ne 1 ] || [ -n "$marker_byte" ] \
            || ! printf 'retired\n' >&"$retirement_fd"; then
        log "FAILED: cannot retire container create-operation lease"
        exec {retirement_fd}>&-
        return 1
    fi
    if ! exec {retirement_fd}>&-; then
        log "FAILED: cannot close retired container create-operation lease"
        return 1
    fi
}

cleanup() {
    original_rc=$?
    trap - EXIT
    final_rc=$original_rc
    if [ "$CREATE_OP_STARTED" = true ] && [ "$CREATE_OP_QUIESCENT" != true ]; then
        if quiesce_create_operation; then
            :
        else
            quiesce_rc=$?
            log "FAILED: container create operation did not reach quiescence; refusing an absence claim"
            [ "$final_rc" -ne 0 ] || final_rc=$quiesce_rc
        fi
    fi
    if [ -n "$CREATE_OUTPUT_PATH" ] \
            && ! rm -f -- "$CREATE_OUTPUT_PATH"; then
        log "FAILED: could not remove container create output $CREATE_OUTPUT_PATH"
        [ "$final_rc" -ne 0 ] || final_rc=1
    fi
    if [ "$CREATE_OP_STARTED" = true ] \
            && [ "$CREATE_OP_QUIESCENT" = true ] \
            && [ "$CREATE_OUTCOME_CHECKED" != true ] \
            && [ "$CONTAINER_OWNERSHIP_VERIFIED" != true ]; then
        if adopt_completed_create; then
            :
        else
            adopt_rc=$?
            if [ "$adopt_rc" -eq 2 ]; then
                log "leaving same-name container $CNAME: owner label is not this invocation"
            elif [ "$final_rc" -eq 0 ]; then
                log "FAILED: successful run lost its owned container identity"
                final_rc=1
            fi
        fi
        CREATE_OUTCOME_CHECKED=true
    fi
    if [ "$CREATE_OP_QUIESCENT" = true ] && ! remove_owned_container; then
        [ "$final_rc" -ne 0 ] || final_rc=1
    fi
    if [ -n "$CREATE_OP_LOCK_FD" ]; then
        exec {CREATE_OP_LOCK_FD}>&-
        CREATE_OP_LOCK_FD=
    fi
    if [ "$final_rc" -ne 0 ]; then
        if ! withdraw_failed_run \
                "hostcdp exited $final_rc; raw completion is not authorized"; then
            log "FAILED: withdrawal of refused run was incomplete"
        fi
    fi
    exit "$final_rc"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# BENCH_RESOLVE_ALL_TO=<ip> is the resolver rule the VM arm bakes into its
# golden through reqbench.sh GUEST_ENV; the same variable goes to this
# container with -e (entry.sh assembles the Chromium flag from it), so the
# host control runs under the A/B's one variable too. Recorded in run.json as
# resolve_all_to, null when unset.
resolve_env=()
if [ -n "${BENCH_RESOLVE_ALL_TO:-}" ]; then
    resolve_env=(-e "BENCH_RESOLVE_ALL_TO=$BENCH_RESOLVE_ALL_TO")
fi
resolve_json=$(python3 -c 'import json, sys; print(json.dumps(sys.argv[1] or None))' \
    "${BENCH_RESOLVE_ALL_TO:-}")
urls_json=$(python3 -c 'import json, sys; print(json.dumps(sys.argv[1:]))' "${URLS[@]}")

resolved_image_id=$(resolve_image_id "$IMAGE") || exit 2
if [ -n "$IMAGE_ID" ]; then
    requested_image_id=$(canonicalize_image_id "$IMAGE_ID") \
        || { log "REFUSING: IMAGE_ID is not a sha256 identity"; exit 2; }
    [ "$resolved_image_id" = "$requested_image_id" ] || {
        log "REFUSING: image $IMAGE resolves to $resolved_image_id, expected IMAGE_ID=$IMAGE_ID"
        exit 2
    }
    runtime_image_id="$requested_image_id"
else
    runtime_image_id="$resolved_image_id"
fi
readonly runtime_image_id

log "starting host container ($IMAGE at $runtime_image_id) with CDP on $CDP_PORT"
cpus_arg=()
cpus_json=null
if [ "$CPU_BUDGET" = vm-matched ]; then
    cpus_arg=(--cpus "$CPUS")
    cpus_json=$(python3 -c '
import json, math, sys
value = float(sys.argv[1])
if not math.isfinite(value) or value <= 0:
    raise SystemExit("REFUSING: CPUS must be positive and finite")
print(json.dumps(int(value) if value.is_integer() else value))
' "$CPUS") || exit 2
fi

supervisor_parent_pid=$BASHPID
readonly supervisor_parent_pid
exec {CREATE_OP_LOCK_FD}>"$CREATE_OP_LOCK_PATH" \
    || { log "REFUSING: cannot open container create-operation lock"; exit 2; }
flock -s "$CREATE_OP_LOCK_FD" \
    || { log "REFUSING: cannot acquire container create-operation lock"; exit 2; }
CREATE_OP_STARTED=true
CREATE_OP_QUIESCENT=false
# Container creation cannot leave an intentional runtime descendant. The
# subreaper therefore treats every still-live create descendant as an
# incomplete operation, including setsid/double-fork children that closed the
# inherited lease FD. Starting the already-owned exact ID happens separately.
CREATE_OUTPUT_PATH="$RESULTS/.container-create-output"
if python3 "$HERE/phase_supervisor.py" \
        --expected-parent "$supervisor_parent_pid" \
        --timeout "$PODMAN_CREATE_TIMEOUT_SECS" \
        --term-grace "$PODMAN_CREATE_KILL_AFTER_SECS" \
        --kill-grace "$PODMAN_CREATE_QUIESCE_TIMEOUT_SECS" \
        --pass-fd "$CREATE_OP_LOCK_FD" -- \
        podman create --name "$CNAME" \
        --label "$OWNER_LABEL_KEY=$CONTAINER_OWNER_TOKEN" --network host \
        "${cpus_arg[@]}" "${resolve_env[@]}" "$runtime_image_id" \
        >"$CREATE_OUTPUT_PATH"; then
    podman_create_rc=0
else
    podman_create_rc=$?
fi
# The subreaper is the create-completion proof. The exclusive lease is a
# second boundary shared with the outer campaign sweep.
if quiesce_create_operation; then
    :
else
    quiesce_rc=$?
    exit "$quiesce_rc"
fi
podman_create_output=$(<"$CREATE_OUTPUT_PATH")
rm -f -- "$CREATE_OUTPUT_PATH" \
    || { log "REFUSING: cannot remove container create output"; exit 2; }
CREATE_OUTPUT_PATH=

if [ "$podman_create_rc" -eq 0 ]; then
    if ! record_owned_container_id "$podman_create_output"; then
        log "REFUSING: podman create returned no exact container ID: $podman_create_output"
        exit 1
    fi
    if ! inspect_owned_container "$CONTAINER_ID"; then
        log "REFUSING: created container $CONTAINER_ID does not carry this invocation's owner label"
        exit 1
    fi
    verify_container_image "$CONTAINER_ID" || exit 1
    CREATE_OUTCOME_CHECKED=true
    exec {CREATE_OP_LOCK_FD}>&-
    CREATE_OP_LOCK_FD=
else
    log "FAILED: podman create exited $podman_create_rc; cleanup will adopt only owner token $CONTAINER_OWNER_TOKEN"
    exit "$podman_create_rc"
fi

if timeout --kill-after="${PODMAN_CREATE_KILL_AFTER_SECS}s" \
        "${PODMAN_CREATE_TIMEOUT_SECS}s" \
        podman start -- "$CONTAINER_ID" >/dev/null; then
    :
else
    podman_start_rc=$?
    log "FAILED: podman start exited $podman_start_rc for owned container $CONTAINER_ID"
    exit "$podman_start_rc"
fi

# Ready = the same two conditions the VM golden gates on: warm marker file AND
# a live CDP round trip that finds a page target (cdp_health inside the image).
t0=$SECONDS
until timeout --kill-after=5s 30s podman exec "$CONTAINER_ID" test -f /run/bench-ready 2>/dev/null \
        && container_owns_cdp; do
    [ $((SECONDS - t0)) -lt 120 ] || {
        log "container never became ready with an owned CDP listener"
        timeout --kill-after=5s 30s podman logs --tail 20 "$CONTAINER_ID" >&2 || true
        exit 1
    }
    sleep 0.5
done
log "warm marker up after $((SECONDS - t0))s; measuring $REPS reps after $WARMUP warmup ($((WARMUP + REPS)) total) against $URL"

# Record the measured configuration beside the numbers, not in prose. Each row
# below carries this file's exact SHA-256, so a copied jsonl cannot acquire a
# different run's image, resolver, corpus, or count metadata by co-location.
python3 - "$RESULTS/run.json" "$RUNID" "$IMAGE" "$runtime_image_id" "$REPS" "$WARMUP" \
        "$URL" "$CDP_PORT" "$urls_json" "$cpus_json" "$resolve_json" "$la" \
        "$host_boot_id" "$host_machine" "$host_kernel" "$source_revision" \
        "$harness_sha256" "$hostcdp_sha256" "$phase_supervisor_sha256" \
        "$host_resource_finalizer_sha256" \
        "$runtime_bundle_sha256" \
        "$corpus_extra_runtime_bundle_sha256" "$COMPARISON_LABEL" "$CPU_BUDGET" \
        "$CONTAINER_OWNER_TOKEN" "$CONTAINER_ID" <<'PY'
import json
import os
import sys
import tempfile

(output_path, run_id, image, image_id, reps, warmup, url, cdp_port,
 urls_json, cpus_json, resolve_json, loadavg, host_boot_id, host_machine,
 host_kernel, source_revision, harness_sha256, hostcdp_sha256,
 phase_supervisor_sha256, host_resource_finalizer_sha256,
 runtime_bundle_sha256, corpus_extra_runtime_bundle_sha256,
 comparison_label, cpu_budget, owner_token, container_id) = sys.argv[1:]
urls = json.loads(urls_json)
record = {
    "run_id": run_id,
    "image": image,
    "image_id": image_id,
    "reps": int(reps),
    "warmup": int(warmup),
    "total_reps": int(reps) + int(warmup),
    "url": url,
    "cdp_port": int(cdp_port),
    "urls": urls,
    "url_count": len(urls),
    "cpus": json.loads(cpus_json),
    "cpu_budget": cpu_budget,
    "driver": "cdpdrive.py",
    "network": "host (no VM, no DNAT)",
    "resolve_all_to": json.loads(resolve_json),
    "host_boot_id": host_boot_id,
    "host_machine": host_machine,
    "host_kernel": host_kernel,
    "source_revision": source_revision,
    "harness_sha256": harness_sha256,
    "hostcdp_sha256": hostcdp_sha256,
    "phase_supervisor_sha256": phase_supervisor_sha256,
    "host_resource_finalizer_sha256": host_resource_finalizer_sha256,
    "runtime_bundle_sha256": runtime_bundle_sha256,
    "corpus_extra_runtime_bundle_sha256":
        corpus_extra_runtime_bundle_sha256 or None,
    "comparison_label": comparison_label,
    "container_owner_token": owner_token,
    "container_id": container_id,
    "loadavg_at_start": loadavg,
}
directory = os.path.dirname(output_path)
fd, temporary = tempfile.mkstemp(prefix=".run.", dir=directory)
try:
    with os.fdopen(fd, "w") as target:
        json.dump(record, target, sort_keys=True)
        target.write("\n")
        target.flush()
        os.fsync(target.fileno())
    os.replace(temporary, output_path)
    directory_fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(directory_fd)
    finally:
        os.close(directory_fd)
except BaseException:
    try:
        os.unlink(temporary)
    except FileNotFoundError:
        pass
    raise
PY
run_json_sha256=$(sha256sum "$RESULTS/run.json" | cut -d' ' -f1)

OUT="$RESULTS/hostcdp.jsonl"
: > "$OUT"
TOTAL_REPS=$((WARMUP + REPS))
python3_command=$(command -v python3)
for rep in $(seq 0 $((TOTAL_REPS - 1))); do
    rep_url="${URLS[$((rep % ${#URLS[@]}))]}"
    rep_tmp=$(mktemp -d "$RESULTS/.rep-${rep}.XXXXXX") \
        || { log "REFUSING: cannot create timing workspace for rep $rep"; exit 5; }
    if python3 - "$python3_command" "$HERE/cdpdrive.py" \
            "127.0.0.1:$CDP_PORT" "$rep_url" \
            "$rep_tmp/output" "$rep_tmp/wall_ms" <<'PY'
import subprocess
import sys
import time

(python_executable, driver, address, url,
 output_path, elapsed_path) = sys.argv[1:]
started = time.monotonic_ns()
result = subprocess.run(
    [python_executable, driver, address, url, "--format", "jpeg", "--nav-timing"],
    stdout=subprocess.PIPE,
    stderr=subprocess.STDOUT,
)
elapsed_ms = (time.monotonic_ns() - started) / 1_000_000
with open(output_path, "wb") as output:
    output.write(result.stdout)
with open(elapsed_path, "w") as timing:
    timing.write(f"{elapsed_ms:.1f}\n")
raise SystemExit(result.returncode)
PY
    then
        ok=true
    else
        ok=false
    fi
    if [ ! -f "$rep_tmp/output" ] || [ ! -f "$rep_tmp/wall_ms" ]; then
        rm -rf -- "$rep_tmp"
        log "REFUSING: monotonic timing wrapper produced no record for rep $rep"
        exit 5
    fi
    out=$(<"$rep_tmp/output")
    wall_ms=$(<"$rep_tmp/wall_ms")
    rm -rf -- "$rep_tmp"
    [[ "$wall_ms" =~ ^[0-9]+([.][0-9]+)?$ ]] \
        || { log "REFUSING: monotonic timing wrapper produced invalid elapsed time: $wall_ms"; exit 5; }
    warm=$([ "$rep" -lt "$WARMUP" ] && echo true || echo false)
    # Per-rep 1-minute load, the same field reqbench.py puts on every record
    # (rec["loadavg1"]) and reqanalyze reports as min/median/max "during run".
    # The start-of-run reading in run.json cannot show contention that arrived
    # mid-run, which is the contention that would move these numbers. Preserve
    # the raw read and its status even when invalid, then refuse the run without
    # publishing a summary.
    if la_rep_raw=$(cut -d' ' -f1 "$LOADAVG_FILE" 2>&1); then
        la_rep_status=0
    else
        la_rep_status=$?
    fi
    if [ "$la_rep_status" -eq 0 ] \
            && [[ "$la_rep_raw" =~ ^[0-9]+([.][0-9]+)?$ ]]; then
        la_rep=$(python3 -c 'import json,sys; print(json.dumps(float(sys.argv[1])))' \
            "$la_rep_raw")
        measurement_valid=true
    else
        la_rep=null
        measurement_valid=false
    fi
    printf '{"run_json_sha256": "%s", "rep": %d, "ok": %s, "warmup": %s, "wall_ms": %s, "loadavg1": %s, "loadavg1_raw": %s, "loadavg1_read_status": %d, "measurement_valid": %s, "url": %s, "driver": %s}\n' \
        "$run_json_sha256" "$rep" "$ok" "$warm" "$wall_ms" "$la_rep" \
        "$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1][-2000:]))' "$la_rep_raw")" \
        "$la_rep_status" "$measurement_valid" \
        "$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "$rep_url")" \
        "$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1][-2000:]))' "$out")" >> "$OUT"
    if [ "$measurement_valid" != true ]; then
        log "REFUSING: rep $rep has no numeric 1-minute load from $LOADAVG_FILE (status=$la_rep_status raw=${la_rep_raw:0:200})"
        exit 5
    fi
    [ "$ok" = true ] || { log "rep $rep FAILED ($rep_url): $out"; exit 4; }
done

if [ -n "$SOURCE_REVISION" ]; then
    source_revision_after="$SOURCE_REVISION"
else
    source_revision_after=$(git -C "$REPO" rev-parse HEAD) \
        || { log "REFUSING: cannot re-read source revision"; exit 5; }
fi
harness_sha256_after=$(compute_harness_sha256) \
    || { log "REFUSING: cannot re-hash request harness sources"; exit 5; }
hostcdp_sha256_after=$(sha256sum "$HERE/hostcdp.sh" | cut -d' ' -f1) \
    || { log "REFUSING: cannot re-hash hostcdp.sh"; exit 5; }
phase_supervisor_sha256_after=$(sha256sum "$HERE/phase_supervisor.py" | cut -d' ' -f1) \
    || { log "REFUSING: cannot re-hash phase_supervisor.py"; exit 5; }
host_resource_finalizer_sha256_after=$(sha256sum "$HERE/host_resource_finalizer.py" | cut -d' ' -f1) \
    || { log "REFUSING: cannot re-hash host_resource_finalizer.py"; exit 5; }
if [ "$source_revision_after" != "$source_revision" ] \
        || [ "$harness_sha256_after" != "$harness_sha256" ] \
        || [ "$hostcdp_sha256_after" != "$hostcdp_sha256" ] \
        || [ "$phase_supervisor_sha256_after" != "$phase_supervisor_sha256" ] \
        || [ "$host_resource_finalizer_sha256_after" != "$host_resource_finalizer_sha256" ]; then
    log "REFUSING: producer source changed during the measured run"
    exit 5
fi
if [ -n "$REQBENCH_RUNTIME_MANIFEST" ]; then
    verify_runtime_manifest "$REQBENCH_RUNTIME_MANIFEST" \
        "$runtime_bundle_sha256" reqbench || exit 5
else
    runtime_bundle_sha256_after=$(compute_live_reqbench_bundle_sha256) \
        || { log "REFUSING: cannot re-hash the live reqbench runtime"; exit 5; }
    [ "$runtime_bundle_sha256_after" = "$runtime_bundle_sha256" ] || {
        log "REFUSING: reqbench runtime changed during the measured run"
        exit 5
    }
fi
if [ -n "$CORPUS_EXTRA_RUNTIME_MANIFEST" ]; then
    verify_runtime_manifest "$CORPUS_EXTRA_RUNTIME_MANIFEST" \
        "$corpus_extra_runtime_bundle_sha256" corpus-extra || exit 5
fi
resolved_image_id_after=$(resolve_image_id "$IMAGE") || exit 5
[ "$resolved_image_id_after" = "$runtime_image_id" ] || {
    log "REFUSING: image $IMAGE changed from $runtime_image_id to $resolved_image_id_after during the run"
    exit 5
}
verify_container_image "$CONTAINER_ID" || exit 5
remove_owned_container || exit 5
retire_create_operation || exit 5

python3 - "$OUT" "$WARMUP" "$RESULTS/.summary.pending" <<'PY'
import json
import os
import statistics
import sys
import tempfile
rows = [json.loads(l) for l in open(sys.argv[1])]
measured_rows = [r for r in rows if not r["warmup"]]


def pct(values, p):
    """p50 is statistics.median, which is what reqanalyze uses for every median
    it publishes, so the descriptive host and VM tables use the same
    convention. Separately timed runs do not publish an effect ratio. Other
    percentiles are nearest-rank."""
    values = sorted(values)
    n = len(values)
    if p == 50:
        return statistics.median(values)
    return values[max(0, -(-p * n // 100) - 1)]


measured = [r["wall_ms"] for r in measured_rows]
n = len(measured)
if n == 0:
    # REPS >= 1 is enforced before any rep runs and measured == REPS, so this
    # can only mean the jsonl and the warmup count disagree. Refuse rather than
    # die on an IndexError after minutes of work with no summary beside the
    # jsonl.
    sys.exit(f"REFUSING: no measured reps in {len(rows)} rows (warmup={sys.argv[2]}); nothing to summarise")
p50, p95 = pct(measured, 50), pct(measured, 95)
print(f"host direct-CDP warm pool: n={n} p50={p50:.1f}ms p95={p95:.1f}ms "
      f"mean={statistics.mean(measured):.1f}ms failures=0")
per_url = {}
if any("url" in r for r in measured_rows):
    by_url = {}
    for r in measured_rows:
        by_url.setdefault(r.get("url", ""), []).append(r["wall_ms"])
    for url, vals in by_url.items():
        per_url[url] = {"n": len(vals), "p50_ms": round(pct(vals, 50), 1),
                        "p95_ms": round(pct(vals, 95), 1),
                        "mean_ms": round(statistics.mean(vals), 1)}
    if len(by_url) > 1:
        print("per-url wall p50 (ms):")
        for url, s in sorted(per_url.items(), key=lambda kv: kv[1]["p50_ms"]):
            print(f"  {s['p50_ms']:8.1f}  n={s['n']:3d}  {url}")
# Name the convention in the record, not only in the code that produced it: a
# ratio between this p50 and a reqanalyze median is only meaningful if both are
# statistics.median, and a reader of summary.json alone cannot otherwise tell.
la = [r["loadavg1"] for r in measured_rows if isinstance(r.get("loadavg1"), (int, float))]
load = None
if la:
    load = {"n": len(la), "min": round(min(la), 2),
            "median": round(statistics.median(la), 2), "max": round(max(la), 2)}
    print(f"loadavg1 during measured reps: min={load['min']} median={load['median']} "
          f"max={load['max']}   <-- contention check")
summary = {"n": n, "p50_ms": round(p50, 1), "p95_ms": round(p95, 1),
           "mean_ms": round(statistics.mean(measured), 1),
           "failures": 0, "p50_convention": "statistics.median",
           "loadavg1_measured": load, "per_url": per_url}
output_path = sys.argv[3]
directory = os.path.dirname(output_path)
fd, temporary = tempfile.mkstemp(prefix=".summary.", dir=directory)
try:
    with os.fdopen(fd, "w") as target:
        json.dump(summary, target, indent=1)
        target.write("\n")
        target.flush()
        os.fsync(target.fileno())
    os.replace(temporary, output_path)
    directory_fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(directory_fd)
    finally:
        os.close(directory_fd)
except BaseException:
    try:
        os.unlink(temporary)
    except FileNotFoundError:
        pass
    raise
PY
log "results in $RESULTS"
