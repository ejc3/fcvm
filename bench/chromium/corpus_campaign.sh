#!/usr/bin/env bash
# Corpus campaign: golden + gated measured run over Cloudflare's 14-URL corpus.
#
# The report's publication rule is that published numbers come from this corpus
# mix and nothing else; medium.html is a micro-benchmark for optimisation work.
# The procedure was never written down, so regenerating it meant reverse
# engineering the invocation out of a sealed analysis.json. This is that
# procedure, written down.
#
# Wiring. The guest renders the corpus from a host-side byte replay:
#
#   guest Chromium --> resolv.conf 10.0.2.2 (baked into the golden by GUEST_DNS)
#                 --> pasta maps the gateway onto the host's loopback
#                 --> corpus_serve.py on 127.0.0.1: DNS 53, HTTP 80, HTTPS 443
#                 --> wildcard A record answering 10.0.2.2, so EVERY hostname a
#                     page pulls (assets, beacons, subdomains) comes back here
#
# The wildcard is why a dnsmasq `address=/domain/` list is not a substitute, and
# why this needs 127.0.0.1:53 specifically. Ubuntu's dnsmasq owns that socket,
# so the campaign stops it and restarts it on the way out. Host name resolution
# is unaffected: /etc/resolv.conf points at systemd-resolved on 127.0.0.53.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="${REPO:-$(cd "$HERE/../.." && pwd)}"
ENGINE="${ENGINE:-chromium}"
case "$ENGINE" in chromium|webkit) ;; *) echo "unknown ENGINE '$ENGINE'" >&2; exit 2 ;; esac
# Engine-scoped default tag: a webkit golden under the chromium tag would be
# refused by the seal anyway (different image digest), but the failure would
# come minutes in rather than at naming time.
if [ "$ENGINE" = webkit ]; then TAG="${TAG:-cb-req-corpus-webkit}"; else TAG="${TAG:-cb-req-corpus}"; fi
# cdp-fast is CDP WebSocket prewiring; WebKit's driver has no WebSocket, so its
# default drops that arm rather than recording 202 guaranteed failures.
if [ "${ENGINE:-chromium}" = webkit ]; then
    ARMS="${ARMS:-cdp,noop}"
else
    ARMS="${ARMS:-cdp,cdp-fast,noop}"
fi
REPS="${REPS:-202}"
WARMUP="${WARMUP:-28}"          # two full 14-URL cycles; the harness fails closed below this

UFFD_MODE="${UFFD_MODE:-minor}"
UFFD_PREFETCH="${UFFD_PREFETCH:-on}"
BACKEND="${BACKEND:-uffd}"
# Arms reqanalyze's stall gate for the measured run. A corpus page replays
# from the host and its load event completes in well under a second; a
# resolver that never answers holds a navigation for ~30 s. 15 s separates
# the two, and campaign_summary refuses an analysis whose gate was never
# armed, so an unset value here would make every run un-indexable.
STALL_MAX_MS="${STALL_MAX_MS:-15000}"
# The diag phase's load-event limit (reqbench diag, DIAG_MAX_LOAD_MS): the
# same 15 s, for the same reason. DIAG_REPS reaches the diag target through
# the environment when set.
DIAG_MAX_LOAD_MS="${DIAG_MAX_LOAD_MS:-15000}"
# campaign_summary holds the diag's limit to the run's stall gate and
# refuses a diag that was allowed more than the measured run, so a campaign
# that would leave an un-indexable diag stops here, before the golden. Both
# are positive integers of milliseconds; reqanalyze and reqbench check their
# own copies later, but a bad value must not cost a golden first.
for knob in STALL_MAX_MS DIAG_MAX_LOAD_MS; do
    [[ "${!knob}" =~ ^[1-9][0-9]*$ ]] \
        || { echo "BLOCKED: $knob must be a positive integer of milliseconds (got '${!knob}')" >&2; exit 2; }
done
[ "$DIAG_MAX_LOAD_MS" -le "$STALL_MAX_MS" ] \
    || { echo "BLOCKED: DIAG_MAX_LOAD_MS=$DIAG_MAX_LOAD_MS exceeds STALL_MAX_MS=$STALL_MAX_MS; campaign_summary refuses a diag allowed more than the run's stall gate" >&2; exit 2; }
# DIAG_ONLY=1 ends the campaign after golden, verify and diag: no measured
# run, no analysis. For the throwaway golden round.
DIAG_ONLY="${DIAG_ONLY:-0}"
STAMP="$(date +%Y%m%d-%H%M%S)"
RESULTS="${RESULTS:-$REPO/bench/chromium/results/reqbench-$STAMP-corpus}"
LOGDIR="${LOGDIR:-/tmp/corpus-campaign-$STAMP}"
mkdir -p "$LOGDIR"
# Created here, not left to reqbench: the replay server's logs and the resolver
# evidence below are written into it before any reqbench phase runs.
mkdir -p "$RESULTS"
# An explicit RESULTS can be reused. Evidence an earlier campaign left there
# must not outlive this one's start: a clean verdict beside a run this
# campaign never finished would be indexed as if it were this run's, and
# corpus_serve appends to its replay logs, so an earlier attempt's queries and
# requests would be hashed into this run's evidence as its own. The run
# record goes too: reqbench.py appends to reqbench.jsonl, so a retry would
# carry two run_ids and reqanalyze would emit a pooled analysis with no
# top-level cell, and a retry that fails before its own analysis would leave
# the earlier analysis.json beside this attempt's fresh evidence. The diag
# summary goes for the same reason: run_diag reads it back after the
# sub-make, and an earlier campaign's passed=true must not answer for this
# one. The content-addressed runtime bundles under runtime/ and the phase
# logs under logs/ are not the record and stay.
rm -f "$RESULTS"/dns-evidence.json "$RESULTS"/verify-dns*.json "$RESULTS"/dns-owner.log \
    "$RESULTS"/corpus-dns.log "$RESULTS"/corpus-access.log "$RESULTS"/corpus-serve.status \
    "$RESULTS"/reqbench.jsonl "$RESULTS"/analysis.json "$RESULTS"/diag/summary.json

# The 14 URLs, in the order the sealed 2026-08-14 run cycled them. Order is part
# of the schedule: reqanalyze re-derives the expected URL per record from it, so
# a reordering is a different experiment, not a cosmetic change.
URLS="https://example.com/,https://news.ycombinator.com/,https://developers.cloudflare.com/,https://blog.cloudflare.com/,https://en.wikipedia.org/,https://developer.mozilla.org/en-US/,https://www.elmundo.es/,https://www.rtp.pt/noticias/,https://www.theguardian.com/international,https://todomvc.com/examples/javascript-es6/dist/,https://todomvc.com/examples/react/dist/index.html,https://todomvc.com/examples/vue/dist/,https://todomvc.com/examples/angular/dist/browser/,https://todomvc.com/examples/preact/dist/"

say() { printf '\n=== %s\n' "$*"; }

# --- resolver evidence -------------------------------------------------------
# The replay wiring above is only evidence if it held for the WHOLE measured
# run. A dnsmasq restart, a corpus_serve leaked from an earlier campaign, or a
# golden that ignored --dns would hand the guest a different resolver, and no
# record would say so. Three brackets close that:
#   1. reqbench's verify (HOP D) asks a RESTORED clone, from inside the
#      container, that every corpus host resolves to 10.0.2.2 and that every
#      corpus URL fetches through that resolver: once before the settle wait,
#      once immediately before the measured run, once after it. Each bracket
#      keeps its evidence as $RESULTS/verify-dns-<stage>.json. After the
#      first bracket, reqbench's diag renders every corpus URL on its own
#      clone with a network trace and refuses the campaign when any request
#      reached an address other than 10.0.2.2, any name did not resolve, or
#      any load event took longer than DIAG_MAX_LOAD_MS
#      ($RESULTS/diag/summary.json).
#   2. a sampler names the owner of 127.0.0.1:53, the dnsmasq state and the
#      1-min load every DNS_SAMPLE_INTERVAL seconds while the run is in
#      flight ($RESULTS/dns-owner.log). The quiet gate reads the load once,
#      before the run; the samples are what says it stayed quiet.
#   3. $RESULTS/dns-evidence.json ties them together with each bracket's
#      sha256 (verify_file_sha256), the replay server's own DNS and access
#      logs (sha256), its exit status
#      (corpus_serve_exit_status, from $RESULTS/corpus-serve.status), the
#      maximum 1-min load over the samples (load_max_1min), and a verdict:
#      "clean" only when all three brackets are present and passed, every
#      sample names this campaign's corpus_serve with dnsmasq inactive, the
#      sampler lived until the run ended, both replay logs exist, the server
#      exited 0 (its logs are complete only then), and dnsmasq is inactive
#      after the clone restores.
engine_target() {
    # $1 = golden | verify | diag | run
    printf 'bench-%s-request-%s' "$([ "$ENGINE" = webkit ] && echo webkit || echo chromium)" "$1"
}

corpus_hosts() {
    # The distinct hostnames of $URLS, in first-seen order.
    printf '%s\n' "$URLS" | tr ',' '\n' | sed -E 's#^https?://([^/]+).*#\1#' \
        | awk 'NF && !seen[$0]++' | paste -sd, -
}

run_verify() {
    # $1 = pre | before-run | after-run. reqbench overwrites verify-dns.json
    # and its verify logs on every call, so each bracket copies its own out
    # under the stage name. A bracket passes only when the sub-make exits 0
    # AND the evidence it left says passed=true.
    local stage="$1" copy="$RESULTS/verify-dns-$1.json" f
    say "verify ($stage): render hops + baked resolver on a restored clone"
    # Both removed first: this function runs as `run_verify X || ...`, where
    # bash keeps errexit off, so an unchecked cp that failed would leave the
    # jq below validating whatever copy was already there.
    rm -f "$RESULTS/verify-dns.json" "$copy"
    if ! VERIFY_DNS_HOSTS="$CORPUS_HOSTS" VERIFY_DNS_ANSWER=10.0.2.2 VERIFY_DNS_URLS="$URLS" \
        TAG="$TAG" ENGINE="$ENGINE" RESULTS="$RESULTS" \
        make -C "$REPO" "$(engine_target verify)" 2>&1 | tee "$LOGDIR/verify-$stage.log"; then
        echo "FAILED: verify ($stage) did not pass; see $LOGDIR/verify-$stage.log" >&2
        if [ -f "$RESULTS/verify-dns.json" ]; then cp "$RESULTS/verify-dns.json" "$copy"; fi
        return 1
    fi
    for f in verify-serve verify-clone verify-clone2 verify-dns; do
        if [ -f "$RESULTS/logs/$f.log" ]; then cp "$RESULTS/logs/$f.log" "$RESULTS/logs/$f-$stage.log"; fi
    done
    [ -s "$RESULTS/verify-dns.json" ] \
        || { echo "FAILED: verify ($stage) left no $RESULTS/verify-dns.json (HOP D did not run)" >&2; return 1; }
    cp "$RESULTS/verify-dns.json" "$copy" \
        || { echo "FAILED: verify ($stage): cannot keep the evidence as $copy" >&2; return 1; }
    # passed=true is also what HOP D writes when it was given nothing to
    # check; the bracket must show the replay resolver and every corpus host
    # and URL answered through it, with the URL probes run under no proxy
    # (proxies_disabled): fc-agent injects the host's HTTP_PROXY/HTTPS_PROXY
    # into the exec, and a probe that honoured them would have fetched the
    # live site with the replay resolver never consulted.
    jq -e --arg hosts "$CORPUS_HOSTS" --arg urls "$URLS" '
        . as $e
        | .passed == true and .dns_server == "10.0.2.2" and .proxies_disabled == true
        and all($hosts | split(",")[]; . as $h | $e.hosts[$h].ok == true)
        and all($urls | split(",")[]; . as $u | $e.urls[$u].ok == true)' "$copy" >/dev/null \
        || { echo "FAILED: verify ($stage): $copy does not record passed=true through 10.0.2.2 with proxies disabled for every corpus host and URL" >&2; return 1; }
}

run_diag() {
    # What holds each corpus page's load event, asked of restored clones of
    # the golden the run will use, on the run's backend, before anything is
    # measured. Every request must reach the replay (10.0.2.2), every name
    # must resolve, every load event must land inside DIAG_MAX_LOAD_MS. The
    # sub-make must exit 0 AND the summary it leaves must say passed=true:
    # a stale summary from an earlier campaign is removed at start.
    local out="$RESULTS/diag/summary.json" expect=10.0.2.2
    # Only Chromium's render carries a network trace; reqbench refuses an
    # IP expectation it cannot enforce, so the WebKit diag is asked for
    # load events and render success alone, and the verify brackets hold
    # the resolver evidence for both engines.
    [ "$ENGINE" = webkit ] && expect=""
    # The diag's serve runs with working-set replay off, whatever the
    # measured run will use. With it on, the UFFD server records every
    # clone's faults into memory.bin.working-set beside the golden, and the
    # run would replay the working set of the diag's renders (every corpus
    # URL, DIAG_REPS clones each) instead of the golden's own: a fresh golden
    # would measure like a reused one, and the PHASE comparison below would
    # be two warmed sidecars. off records nothing and writes no file;
    # reqbench refuses anything else for the diag, and campaign_summary
    # refuses a diag summary that does not say off. Set on the make line so
    # it also beats a UFFD_PREFETCH exported into this campaign's environment.
    say "diag: one traced render per corpus URL on restored clones of $TAG, replay off (limit ${DIAG_MAX_LOAD_MS} ms, expect ${expect:-<no trace>})"
    if ! DIAG_URLS="$URLS" DIAG_EXPECT_IPS="$expect" DIAG_MAX_LOAD_MS="$DIAG_MAX_LOAD_MS" \
        TAG="$TAG" ENGINE="$ENGINE" RESULTS="$RESULTS" BACKEND="$BACKEND" \
        UFFD_MODE="$UFFD_MODE" UFFD_PREFETCH=off \
        make -C "$REPO" "$(engine_target diag)" 2>&1 | tee "$LOGDIR/diag.log"; then
        echo "FAILED: diag did not pass; see $LOGDIR/diag.log and $out" >&2
        return 1
    fi
    jq -e '.passed == true' "$out" >/dev/null 2>&1 \
        || { echo "FAILED: diag left no $out saying passed=true" >&2; return 1; }
}

DNS_SAMPLE_INTERVAL="${DNS_SAMPLE_INTERVAL:-10}"
# The same override reqbench.sh's quiet gate takes; tests point it at a file.
LOADAVG_FILE="${LOADAVG_FILE:-/proc/loadavg}"
SAMPLER_PID=""
SAMPLER_ALIVE_AT_STOP=""

dns_owner_sample() {
    # One line: "<utc ts> owner_pid=<pid|none> dnsmasq=<state> load1=<1-min
    # load>". The local address is matched exactly: systemd-resolved
    # (127.0.0.53) and dnsmasq's per-interface listeners share the port. sudo
    # is what makes ss show the pid behind a root-owned socket; -n so a
    # missing grant fails the sample instead of hanging the sampler on a
    # prompt. The load is the first field of LOADAVG_FILE and must be a
    # number: a column that silently went missing would read as a quiet
    # box. A sample that cannot be taken returns 1 and ends the sampler
    # (below), which the evidence records as unclean.
    local listing owner state load1
    listing=$(sudo -n ss -lnup 'sport = :53' 2>/dev/null) || return 1
    owner=$(awk '$4 == "127.0.0.1:53" && match($0, /pid=[0-9]+/) { print substr($0, RSTART + 4, RLENGTH - 4); exit }' <<<"$listing")
    state=$(systemctl is-active dnsmasq 2>/dev/null) || state="${state:-unknown}"
    load1=$(cut -d' ' -f1 "$LOADAVG_FILE" 2>/dev/null) || return 1
    case "$load1" in
        '' | *[!0-9.]* | .* | *. | *.*.*) return 1 ;;
    esac
    printf '%s owner_pid=%s dnsmasq=%s load1=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "${owner:-none}" "$state" "$load1"
}

dns_owner_sampler() {
    # $1 = log; runs until killed, or until a sample cannot be taken.
    while :; do
        dns_owner_sample >>"$1" || exit 1
        sleep "$DNS_SAMPLE_INTERVAL"
    done
}

start_dns_sampler() {
    : >"$RESULTS/dns-owner.log"
    SAMPLER_ALIVE_AT_STOP=""
    dns_owner_sampler "$RESULTS/dns-owner.log" &
    SAMPLER_PID=$!
}

stop_dns_sampler() {
    # The sampler's exit status says what ended it: 143 is this SIGTERM,
    # anything else means it died on its own before the run ended, and the
    # samples it left cover only part of the run. `kill -0` cannot tell
    # the two apart (a dead, unreaped child still answers it).
    [ -n "$SAMPLER_PID" ] || return 0
    local status=0
    kill "$SAMPLER_PID" 2>/dev/null || true
    wait "$SAMPLER_PID" 2>/dev/null || status=$?
    if [ "$status" -eq 143 ]; then SAMPLER_ALIVE_AT_STOP=yes; else SAMPLER_ALIVE_AT_STOP=no; fi
    SAMPLER_PID=""
}

dns_first_mismatch() {
    # $1 = sample log, $2 = the pid every sample must name. Prints the first
    # line naming another owner or a dnsmasq state other than inactive.
    awk -v pid="$2" '$2 != "owner_pid=" pid || $3 != "dnsmasq=inactive" { print; exit }' "$1"
}

dns_load_stats() {
    # $1 = sample log. Prints "<count> <max>": how many samples carry a
    # numeric load1= field and the largest value among them, as the sample
    # wrote it (a JSON number as-is), or "0 null" when none does.
    awk '{
        for (i = 1; i <= NF; i++) {
            if (substr($i, 1, 6) != "load1=") continue
            v = substr($i, 7)
            if (v ~ /^[0-9]+(\.[0-9]+)?$/) { n++; if (n == 1 || v + 0 > max + 0) max = v }
        }
    } END { printf "%d %s\n", n, (n ? max : "null") }' "$1"
}

sha256_or_empty() {
    if [ -f "$1" ]; then sha256sum "$1" | cut -d' ' -f1; fi
}

write_dns_evidence() {
    # $1 = clean | unclean from the verify brackets, $2 = optional reason.
    # Everything checked here can only lower the verdict, never lift it:
    # the samples and whether the sampler lived to the stop, the dnsmasq
    # state, the three bracket files and the two replay logs. The load
    # maximum is reported, not judged: the run driver's own gate refused a
    # busy box at the start, and the record is what says how busy it got.
    # Prints the final verdict; a verdict that could not be written is
    # unclean and the function returns 1.
    local verdict="$1" reason="${2:-}" log="$RESULTS/dns-owner.log"
    local out="$RESULTS/dns-evidence.json"
    local samples=0 first_mismatch="" before=false after=false after_state f
    local sampler_alive=false load_stats load_samples=0 load_max=null
    [ "$DNSMASQ_WAS_ACTIVE" = yes ] && before=true
    [ "$SAMPLER_ALIVE_AT_STOP" = yes ] && sampler_alive=true
    if [ -s "$log" ]; then
        samples=$(wc -l <"$log")
        first_mismatch=$(dns_first_mismatch "$log" "$SERVE_PID")
        load_stats=$(dns_load_stats "$log")
        load_samples=${load_stats%% *}
        load_max=${load_stats##* }
    fi
    if [ "$samples" -eq 0 ]; then
        verdict=unclean; reason="${reason:-no 127.0.0.1:53 owner samples were taken}"
    elif [ -n "$first_mismatch" ]; then
        verdict=unclean; reason="${reason:-a sample did not name corpus_serve $SERVE_PID with dnsmasq inactive}"
    elif [ "$sampler_alive" != true ]; then
        verdict=unclean; reason="${reason:-the :53 owner sampler died before the measured run ended}"
    fi
    # "after restore" is the clone restores of the measured run: read before
    # the exit trap starts dnsmasq again. Only "inactive" is clean: active
    # means something restarted it while clones were being measured, and
    # failed, activating, unknown or no answer from systemd is not evidence
    # of absence.
    after_state=$(systemctl is-active dnsmasq 2>/dev/null) || true
    after_state="${after_state:-unknown}"
    [ "$after_state" = active ] && after=true
    if [ "$after_state" != inactive ]; then
        verdict=unclean; reason="${reason:-dnsmasq is $after_state after the clone restores, not inactive}"
    fi
    local -a files=()
    # Each bracket is pinned by the sha256 of the bytes this verdict read.
    # The bracket file is the only record that a restored clone resolved the
    # corpus through the replay server, and nothing else hashes it, so
    # without this a file edited after the campaign is indistinguishable from
    # the one the verdict accepted. A failed jq assigns empty output, which
    # --argjson would refuse, so the map goes back to {} and the verdict is
    # unclean.
    local verify_sha='{}' sha
    for f in pre before-run after-run; do
        if [ -f "$RESULTS/verify-dns-$f.json" ]; then files+=("$RESULTS/verify-dns-$f.json"); fi
        if ! jq -e '.passed == true' "$RESULTS/verify-dns-$f.json" >/dev/null 2>&1; then
            verdict=unclean; reason="${reason:-the $f verify bracket is missing or did not pass}"
        fi
        sha=$(sha256_or_empty "$RESULTS/verify-dns-$f.json")
        if [ -z "$sha" ]; then
            verdict=unclean; reason="${reason:-the $f verify bracket cannot be read to hash it}"
        elif ! verify_sha=$(jq -c --arg n "verify-dns-$f.json" --arg s "$sha" \
                '.[$n] = $s' <<<"$verify_sha"); then
            verdict=unclean; reason="${reason:-cannot record the sha256 of the $f verify bracket}"
            verify_sha='{}'
        fi
    done
    for f in corpus-dns.log corpus-access.log; do
        if [ ! -s "$RESULTS/$f" ]; then
            verdict=unclean; reason="${reason:-replay log $f is missing or empty}"
        fi
    done
    # The replay server's exit status, as its wrapper wrote it once the
    # process was gone. 0 is the shutdown sequence completing with both logs
    # closed. 1 is a log line it could not write, with the response already
    # sent, so the logs are short by at least that line; 137 is the SIGKILL
    # escalation; no file means the server is still running, never started,
    # or its wrapper died with it. Only 0 is clean.
    local status_file="$RESULTS/corpus-serve.status" serve_status="" serve_status_json=null
    if [ -f "$status_file" ]; then serve_status=$(tr -d '[:space:]' <"$status_file"); fi
    if [[ "$serve_status" =~ ^(0|[1-9][0-9]{0,4})$ ]]; then
        serve_status_json=$serve_status
        if [ "$serve_status" -ne 0 ]; then
            verdict=unclean; reason="${reason:-corpus_serve exited $serve_status, not 0, so its replay logs may be short}"
        fi
    else
        verdict=unclean; reason="${reason:-corpus_serve left no exit status in $status_file}"
    fi
    if ! jq -n --argjson serve_pid "${SERVE_PID:-null}" --argjson before "$before" --argjson after "$after" \
        --arg after_state "$after_state" --argjson sampler_alive "$sampler_alive" \
        --argjson samples "$samples" --argjson interval "$DNS_SAMPLE_INTERVAL" \
        --argjson load_max "$load_max" --argjson load_samples "$load_samples" \
        --arg first "$first_mismatch" --arg reason "$reason" --arg owner_log "$log" \
        --arg dns_sha "$(sha256_or_empty "$RESULTS/corpus-dns.log")" \
        --arg access_sha "$(sha256_or_empty "$RESULTS/corpus-access.log")" \
        --argjson serve_status "$serve_status_json" \
        --argjson verify_sha "$verify_sha" \
        --arg verdict "$verdict" --args \
        '{serve_pid: $serve_pid, dnsmasq_was_active_before: $before,
          dnsmasq_active_after_restore: $after,
          dnsmasq_state_after_restore: $after_state,
          sampler_alive_at_stop: $sampler_alive, samples: $samples,
          sample_interval_s: $interval, owner_log: $owner_log,
          load_max_1min: $load_max, load_samples: $load_samples,
          first_mismatch: (if $first == "" then null else $first end),
          verify_files: $ARGS.positional,
          verify_file_sha256: $verify_sha,
          corpus_dns_log_sha256: (if $dns_sha == "" then null else $dns_sha end),
          corpus_access_log_sha256: (if $access_sha == "" then null else $access_sha end),
          corpus_serve_exit_status: $serve_status,
          reason: (if $reason == "" then null else $reason end),
          verdict: $verdict}' "${files[@]}" >"$out.tmp" \
        || ! mv "$out.tmp" "$out"; then
        rm -f "$out.tmp"
        echo "FAILED: cannot write $out" >&2
        printf 'unclean\n'
        return 1
    fi
    printf '%s\n' "$verdict"
}

campaign_fail() {
    # A bracket failed before the measured run: record the unclean verdict so
    # the run directory says why it holds no records, then exit.
    write_dns_evidence unclean "$1" >/dev/null
    echo "FAILED: $1" >&2
    exit 1
}

# Every phase runs with debug logging: the stage attribution the analysis relies
# on only exists in fcvm=debug output, and a caller who omitted it would produce
# a run whose records cannot be audited afterwards. Exported once here rather
# than repeated per command, so no phase can be left out.
# Forced, not defaulted. `${RUST_LOG:-...}` let a caller with RUST_LOG=info in
# their shell produce a campaign with no stage records at all -- and nothing
# downstream would say so, because a missing stage looks the same as a stage
# that took no time. The one variable the analysis cannot do without is exactly
# the one an ambient environment is most likely to have already set.
if [ -n "${RUST_LOG:-}" ] && [ "${RUST_LOG}" != "fcvm=debug" ]; then
    echo "note: overriding inherited RUST_LOG=${RUST_LOG} with fcvm=debug (stage attribution needs it)" >&2
fi
export RUST_LOG="fcvm=debug"

# Pin the binary, and say which one. The seal already refuses a golden recorded
# under a different runtime bundle, so a rebuild mid-campaign fails the run
# rather than silently mixing binaries WITHIN one. It does not bind comparisons
# ACROSS runs, and that is not hypothetical: on 2026-08-16 a vCPU curve was
# published whose 2 vCPU points came from fcvm 3976d0ba and whose 4 and 8 vCPU
# points came from 3f85bd26, because the tree was rebuilt between the two
# sweeps. Recording the hash up front makes the boundary visible in the log as
# well as in each run's cell.
FCVM_BIN="${FCVM_BIN:-$REPO/target/release/fcvm}"
[ -x "$FCVM_BIN" ] || { echo "BLOCKED: no fcvm binary at $FCVM_BIN; run make build" >&2; exit 2; }
FCVM_SHA=$(sha256sum "$FCVM_BIN" | cut -d" " -f1)

# --- preflight -------------------------------------------------------------
# The run driver enforces its own quiet gate and would void the record anyway;
# failing here costs seconds instead of the golden's minutes.
load1=$(awk '{print $1}' "$LOADAVG_FILE")
if awk -v l="$load1" 'BEGIN{exit !(l > 2.0)}'; then
    echo "BLOCKED: 1-min load $load1 exceeds the quiet gate (2.0)" >&2
    exit 2
fi
if pgrep -x fcvm >/dev/null 2>&1 || pgrep -x firecracker >/dev/null 2>&1; then
    echo "BLOCKED: stray fcvm/firecracker processes; the run gate refuses these" >&2
    pgrep -a 'fcvm|firecracker' >&2 || true
    exit 2
fi
for f in corpus_serve.py reqbench.sh; do
    [ -f "$REPO/bench/chromium/$f" ] || { echo "BLOCKED: missing bench/chromium/$f" >&2; exit 2; }
done

# --- host replay server ----------------------------------------------------
DNSMASQ_WAS_ACTIVE=no
if systemctl is-active --quiet dnsmasq 2>/dev/null; then
    DNSMASQ_WAS_ACTIVE=yes
fi
SERVE_PID=""

stop_corpus_serve() {
    # `sudo kill -0`, not bare `kill -0`: corpus_serve runs as root (sudo -b)
    # while this script does not, so an unprivileged liveness probe gets EPERM
    # and the guard is ALWAYS false. The server then survives holding
    # 127.0.0.1:53/80/443, the `systemctl start dnsmasq` in cleanup cannot
    # bind, and the next run picks up the leaked pid and records
    # DNSMASQ_WAS_ACTIVE=no. Called before the evidence is written (so the
    # replay-log hashes name files nothing appends to afterwards) and again
    # from the exit trap, which finds it already gone. On SIGTERM the server
    # stops accepting, waits for the handlers still answering (their access
    # lines follow their responses), closes both logs and exits 0; it exits 1
    # when a log line could not be written, which liveness cannot see, so
    # the exit status is what says the logs are complete. The wrapper that
    # launched the server writes that status to $RESULTS/corpus-serve.status
    # once the server is gone; this waits for the file and write_dns_evidence
    # requires it to say 0. The SIGKILL fallback is for a handler that never
    # finishes; the wrapper records 137 and the verdict is unclean.
    local status_file="$RESULTS/corpus-serve.status"
    [ -n "$SERVE_PID" ] || return 0
    if sudo kill -0 "$SERVE_PID" 2>/dev/null; then
        say "stopping corpus_serve ($SERVE_PID)"
        sudo kill "$SERVE_PID" 2>/dev/null || true
        # Not `wait`: the server is not a child of this shell (sudo -b detached
        # it), so wait returns immediately and proves nothing. Poll instead.
        for _ in $(seq 1 50); do
            sudo kill -0 "$SERVE_PID" 2>/dev/null || break
            sleep 0.1
        done
        if sudo kill -0 "$SERVE_PID" 2>/dev/null; then
            say "corpus_serve $SERVE_PID did not exit; escalating to SIGKILL"
            sudo kill -9 "$SERVE_PID" 2>/dev/null || true
        fi
    fi
    # A wrapper that never writes (killed with the server, never started)
    # leaves no file, which the evidence records as unclean.
    for _ in $(seq 1 50); do
        if [ -f "$status_file" ]; then break; fi
        sleep 0.1
    done
    if [ -f "$status_file" ]; then
        say "corpus_serve $SERVE_PID exit status: $(tr -d '[:space:]' <"$status_file")"
    else
        say "corpus_serve $SERVE_PID left no exit status in $status_file"
    fi
}

cleanup() {
    set +e
    # The sampler is a background subshell of this script; nothing else reaps
    # it when the campaign dies mid-run.
    stop_dns_sampler
    stop_corpus_serve
    if [ "$DNSMASQ_WAS_ACTIVE" = yes ] && ! systemctl is-active --quiet dnsmasq; then
        say "restarting dnsmasq"
        # Retry and REPORT. corpus_serve can still be releasing :53 as this runs
        # (the poll above bounds its exit at 5s, it does not prove the socket is
        # closed), so a single attempt loses the race. Unchecked, this trap left
        # the box with no DNS resolution and said nothing -- the same defect
        # bench-stop had, in the other code path that restores this service.
        #
        # Exiting non-zero FROM the trap is deliberate: a campaign that leaves
        # the host unable to resolve anything has not finished successfully,
        # whatever its measurements say, and the next run's preflight would fail
        # somewhere far less obvious.
        for _ in $(seq 1 10); do
            sudo systemctl start dnsmasq >/dev/null 2>&1 && break
            sleep 1
        done
        if ! systemctl is-active --quiet dnsmasq; then
            echo "FAILED: dnsmasq did not restart; this box has no DNS." >&2
            echo "Something still holds :53 -- check: sudo ss -lnup 'sport = :53'" >&2
            exit 1
        fi
    fi
}
trap cleanup EXIT

if [ "$DNSMASQ_WAS_ACTIVE" = yes ]; then
    say "stopping dnsmasq so corpus_serve can own 127.0.0.1:53"
    sudo systemctl stop dnsmasq
fi

say "starting corpus_serve (DNS 127.0.0.1:53 answering 10.0.2.2; HTTP 80; HTTPS 443)"
# The PID comes from THIS invocation, via a pidfile the wrapper shell writes
# from $! of the server it started. `pgrep -f corpus_serve.py` would match any
# campaign's server, so a concurrent run's cleanup could kill this one's and
# leave its own alive -- and the survivor still holds :53/:80/:443, so the
# next campaign's preflight passes against a server nobody is tracking.
#
# The wrapper is also what keeps the exit status: `sudo -b` detaches the
# server from this shell, so `wait` cannot reach it and a status the server
# exits with (1 after a log line it could not write, with the response bytes
# already sent) would be lost. The wrapper waits for the server and writes
# the status to $RESULTS/corpus-serve.status by rename, so a partial write is
# never read; stop_corpus_serve waits for the file, write_dns_evidence
# requires it to say 0, and campaign_summary refuses evidence that does not
# carry it.
SERVE_PIDFILE="$LOGDIR/corpus_serve.pid"
SERVE_STATUS_FILE="$RESULTS/corpus-serve.status"
rm -f "$SERVE_PIDFILE" "$SERVE_STATUS_FILE"
# --dns-log / --access-log: the server's per-query DNS log and HTTP access
# log, kept with the run; dns-evidence.json records their sha256.
sudo -b sh -c 'python3 "$2" --root "$3" --port 80 --tls-port 443 --dns-addr 127.0.0.1 --dns-port 53 --answer-ip 10.0.2.2 --dns-log "$4" --access-log "$5" & pid=$!; echo "$pid" > "$1"; wait "$pid"; rc=$?; echo "$rc" > "$6.tmp" && mv "$6.tmp" "$6"' \
    _ "$SERVE_PIDFILE" "$REPO/bench/chromium/corpus_serve.py" "$REPO/bench/chromium/corpus-live" \
    "$RESULTS/corpus-dns.log" "$RESULTS/corpus-access.log" "$SERVE_STATUS_FILE" \
    > "$LOGDIR/corpus_serve.log" 2>&1
# `[ -s f ] && break` would be the last command in the body, so on the first
# iteration (pidfile not written yet) it returns 1 and `set -e` kills the whole
# script -- observed: the server started, loaded 781 urls, and the campaign
# exited straight into cleanup.
for _ in $(seq 1 50); do
    if [ -s "$SERVE_PIDFILE" ]; then break; fi
    sleep 0.1
done
SERVE_PID=$(cat "$SERVE_PIDFILE" 2>/dev/null || true)
[ -n "$SERVE_PID" ] || { echo "BLOCKED: corpus_serve did not start; see $LOGDIR/corpus_serve.log" >&2; cat "$LOGDIR/corpus_serve.log" >&2; exit 3; }
sudo kill -0 "$SERVE_PID" 2>/dev/null || { echo "BLOCKED: corpus_serve pid $SERVE_PID is not alive; see $LOGDIR/corpus_serve.log" >&2; cat "$LOGDIR/corpus_serve.log" >&2; exit 3; }

# Prove all three sockets answer before spending minutes on a golden. A replay
# server that loaded zero urls, or a DNS socket that silently lost the bind,
# would otherwise surface as a corpus of 404s inside the guest.
grep -q "loaded [1-9]" "$LOGDIR/corpus_serve.log" || {
    echo "BLOCKED: corpus_serve loaded no urls" >&2; cat "$LOGDIR/corpus_serve.log" >&2; exit 3; }
# Readiness is not the pidfile. The wrapper writes $$ and THEN execs python,
# which still has to load the corpus and bind 53/80/443, so the pid exists
# several seconds before the sockets do. The original fixed `sleep 3` hid that;
# after switching to a pidfile the curl below raced the TLS bind and returned
# 000, which `set -e` turned into a bare exit 7. Poll the sockets instead, so
# the wait is as long as it needs to be and no longer.
# --noproxy '*' on this curl and the one below: both talk to 127.0.0.1 and
# nothing else. With http_proxy/HTTPS_PROXY in the environment curl would go to
# the proxy instead: a working one fetches the live site and passes an
# incomplete replay, an unreachable one returns 000 for a replay that is up.
answer=""
code=""
for _ in $(seq 1 100); do
    answer=$(dig +short +time=2 +tries=1 @127.0.0.1 blog.cloudflare.com A 2>/dev/null | head -1 || true)
    code=$(curl -sk --noproxy '*' -o /dev/null -w '%{http_code}' --max-time 5 \
           --resolve 'blog.cloudflare.com:443:127.0.0.1' https://blog.cloudflare.com/ 2>/dev/null || true)
    if [ "$answer" = "10.0.2.2" ] && [ "$code" = "200" ]; then break; fi
    sudo kill -0 "$SERVE_PID" 2>/dev/null || { echo "BLOCKED: corpus_serve died during startup" >&2; cat "$LOGDIR/corpus_serve.log" >&2; exit 3; }
    sleep 0.2
done
[ "$answer" = "10.0.2.2" ] || { echo "BLOCKED: wildcard DNS answered '$answer', expected 10.0.2.2" >&2; exit 3; }
[ "$code" = "200" ] || { echo "BLOCKED: HTTPS replay returned '$code' for blog.cloudflare.com" >&2; exit 3; }
say "replay server up: DNS -> 10.0.2.2, HTTPS 200"

# Probe EVERY corpus member, not just the one used to detect startup. A corpus
# holding only blog.cloudflare.com passed the check above, and the other 13 URLs
# then failed INSIDE the measured run -- where a replay miss is a render of an
# error page, which is a perfectly plausible-looking fast number rather than an
# error. Cheap: 14 local requests against a server already proven up.
missing=""
for url in $(printf '%s\n' "$URLS" | tr ',' ' '); do
    host=$(printf '%s' "$url" | sed -E 's#^https?://([^/]+).*#\1#')
    ucode=$(curl -sk --noproxy '*' -o /dev/null -w '%{http_code}' --max-time 10 \
            --resolve "$host:443:127.0.0.1" "$url" 2>/dev/null || true)
    case "$ucode" in
    200 | 30[1278]) ;;
    *) missing="$missing\n  $ucode  $url" ;;
    esac
done
if [ -n "$missing" ]; then
    # shellcheck disable=SC2059
    printf "BLOCKED: the corpus does not serve every configured URL:$missing\n" >&2
    echo "A partial corpus measures error pages as renders. Re-record the corpus." >&2
    exit 3
fi
say "corpus complete: all $(printf '%s' "$URLS" | tr ',' '\n' | wc -l) URLs replay locally"
say "fcvm binary $FCVM_BIN sha256=$FCVM_SHA (recorded per run in cell.fcvm_sha256)"

# --- golden ----------------------------------------------------------------
# PHASE=run reuses the installed golden. The working-set sidecar beside the
# snapshot is the reason that is worth doing separately: a freshly created
# golden has none, so the first measured run pays cold-working-set costs that
# every later run does not. Comparing the two is a one-variable experiment;
# the diag between golden and run serves with replay off (run_diag), so it
# leaves that sidecar as it found it.
PHASE="${PHASE:-all}"
CORPUS_HOSTS=$(corpus_hosts)
if [ "$PHASE" = all ]; then
    say "golden $TAG (GUEST_DNS=10.0.2.2 baked into resolv.conf at boot)"
    GUEST_DNS=10.0.2.2 TAG="$TAG" ENGINE="$ENGINE" RESULTS="$RESULTS" \
        make -C "$REPO" "$(engine_target golden)" 2>&1 | tee "$LOGDIR/golden.log"
    run_verify pre || campaign_fail "verify (pre) failed on the new golden"
else
    snap="${DATA_ROOT:-/mnt/fcvm-btrfs}/snapshots/$TAG"
    [ -f "$snap/config.json" ] || { echo "BLOCKED: PHASE=$PHASE but no golden at $snap" >&2; exit 2; }
    ws="$snap/memory.bin.working-set"
    if [ -f "$ws" ]; then
        say "reusing golden $TAG (working-set sidecar present: $(stat -c%s "$ws") bytes, mtime $(stat -c%y "$ws"))"
    else
        say "reusing golden $TAG (NO working-set sidecar: this run records one cold)"
    fi
    # A reused golden is only as good as its resolver still is: the snapshot
    # may predate a corpus change, or have been made without GUEST_DNS.
    run_verify pre || campaign_fail "verify (pre) failed on the reused golden"
fi

# --- diag --------------------------------------------------------------------
# Whichever golden the phase settled on, diagnose it before spending the
# settle wait and the measured run on it. A page that stalls or reaches the
# live internet is a defect of the golden or the replay, not a number.
run_diag || campaign_fail "diag failed on golden $TAG; not measuring"
if [ "$DIAG_ONLY" = 1 ]; then
    say "DIAG_ONLY=1: golden, verify and diag done for $TAG; no measured run ($RESULTS/diag/summary.json)"
    exit 0
fi

# --- settle -----------------------------------------------------------------
# Creating the golden is CPU-heavy (container build, cold boot, snapshot), so
# the box is still hot when this phase ends. The run driver refuses above a
# 1-min load of 2.0 and does not wait, so PHASE=all would throw away the golden
# it just spent minutes building:
#   REFUSING: box is busy (load=2.41, 0 firecracker/fcvm)
# Wait for the average to decay instead. 1.5 leaves margin under the gate, and
# the wait is bounded so a genuinely busy box still fails rather than hanging.
settle_deadline=$(( $(date +%s) + 900 ))
while :; do
    load1=$(awk '{print $1}' "$LOADAVG_FILE")
    if awk -v l="$load1" 'BEGIN{exit !(l < 1.5)}'; then break; fi
    if [ "$(date +%s)" -ge "$settle_deadline" ]; then
        echo "BLOCKED: load stayed at $load1 for 15 min; refusing to measure on a busy box" >&2
        exit 4
    fi
    say "waiting for the box to go quiet (1-min load $load1, need < 1.5)"
    sleep 20
done
say "box quiet (1-min load $load1)"

# The settle wait is time in which the box can change under us; prove the
# resolver again immediately before measuring.
run_verify before-run || campaign_fail "verify (before-run) failed after the settle wait"

# --- measured run ----------------------------------------------------------
say "measured run: $REPS reps/arm, warmup $WARMUP, arms $ARMS, $BACKEND/$UFFD_MODE prefetch=$UFFD_PREFETCH"
start_dns_sampler
run_rc=0
TAG="$TAG" URL="$URLS" BACKEND="$BACKEND" UFFD_MODE="$UFFD_MODE" \
    UFFD_PREFETCH="$UFFD_PREFETCH" ARMS="$ARMS" REPS="$REPS" WARMUP="$WARMUP" \
    STALL_MAX_MS="$STALL_MAX_MS" RESULTS="$RESULTS" ENGINE="$ENGINE" \
    make -C "$REPO" "$(engine_target run)" 2>&1 | tee "$LOGDIR/run.log" || run_rc=$?
stop_dns_sampler

# The run's own exit is not the verdict: a run that measured cleanly against
# the wrong resolver is worse than one that failed, so the after-run bracket
# and the evidence are written whatever the run returned. The replay server
# stops before the evidence hashes its logs.
after_rc=0
run_verify after-run || after_rc=$?
stop_corpus_serve
verdict=$(write_dns_evidence "$([ "$after_rc" -eq 0 ] && echo clean || echo unclean)") || verdict=unclean
say "dns evidence: verdict=$verdict ($RESULTS/dns-evidence.json)"
if [ "$run_rc" -ne 0 ] || [ "$after_rc" -ne 0 ] || [ "$verdict" != clean ]; then
    echo "FAILED: measured run exit $run_rc, after-run verify exit $after_rc, dns verdict $verdict" >&2
    exit 1
fi

say "records: $RESULTS"
say "logs:    $LOGDIR"
