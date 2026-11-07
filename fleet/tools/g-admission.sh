# shellcheck shell=bash
# shellcheck disable=SC2034  # ADMISSION_TAG and the reserve values are read by the scripts that source this file
# g-admission.sh: what every launch of a G instance in the fleet account's host region goes
# through (fleet/tools/gpubox.sh, fleet/ami/build-ami.sh), sourced by both. The wall's GPU host
# shares the account's G quotas there: a launch may take a market only if it leaves room for
# one wall host (96 vCPU), and on demand also for the wall's eight on-demand EC2 tiles (4
# each). Admission is one launch at a time across every machine (an S3 lock), and a launch it
# admits is in a ledger until describe-instances lists it. Every admitted RunInstances carries
# the tag in ADMISSION_TAG; ejc3/aws denies the fleet's Terraform role a G RunInstances without
# it, so a launcher that skips this file (an old checkout's gpubox.sh) is refused by EC2.
#
# The caller defines A (aws in the host region), say, now, BUCKET and S3REGION, and sets
# GA_WHAT (what it launches, for messages) and owner.
#
# On 2026-11-05 the wall's host was refused with VcpuLimitExceeded for 13 minutes on two wakes
# while gpuboxes and an AMI builder held the on-demand quota (ejc3/fcvm#1257); on 2026-11-07 a
# third gpubox launched by a checkout without the reserve, in the second the wall woke, left
# the staging smoke with no fcvm tile. A check that cannot read the quota, the usage, the lock
# or the ledger refuses.
ADMISSION_TAG='{Key=fcvm-admission,Value=wall-reserve}'
WALL_RESERVE_spot=96
WALL_RESERVE_ondemand=128
QUOTA_CODE_spot=L-3819A6DF
QUOTA_CODE_ondemand=L-DB2E81BA
# g_usage MARKET: one line per G or VT instance (both draw on the G quotas) counting against
# that market's quota (pending, running, stopping, shutting down): "<launch time> <instance id>
# <purpose> <vCPUs>". The vCPUs are the type's default, which is what the quota charges
# whatever CPU options an instance was launched with.
g_usage() {
  local lc out types vc
  if [ "$1" = spot ]; then lc='InstanceLifecycle==`spot`'; else lc='InstanceLifecycle!=`spot`'; fi
  out=$(A ec2 describe-instances --filters Name=instance-state-name,Values=pending,running,stopping,shutting-down \
    --query "Reservations[].Instances[?(starts_with(InstanceType, \`g\`) || starts_with(InstanceType, \`vt\`)) && $lc].[LaunchTime, InstanceId, Tags[?Key=='Purpose']|[0].Value, InstanceType]" \
    --output text) || return 1
  awk 'NF && NF != 4 {bad = 1} END {exit bad}' <<<"$out" || return 1
  types=$(awk 'NF {print $4}' <<<"$out" | sort -u)
  [ -n "$types" ] || return 0
  # shellcheck disable=SC2086  # one argument per type
  vc=$(A ec2 describe-instance-types --instance-types $types --query 'InstanceTypes[].[InstanceType, VCpuInfo.DefaultVCpus]' --output text) || return 1
  awk 'NR == FNR {if (NF == 2 && $2 ~ /^[0-9]+$/) v[$1] = $2; next}
       NF {if (!($4 in v)) {bad = 1; next} print $1, $2, $3, v[$4]} END {exit bad}' <(echo "$vc") <(echo "$out")
}
# quota_of MARKET: the market's G vCPU quota, a whole number.
quota_of() {
  local code q
  if [ "$1" = spot ]; then code=$QUOTA_CODE_spot; else code=$QUOTA_CODE_ondemand; fi
  q=$(A service-quotas get-service-quota --service-code ec2 --quota-code "$code" --query 'Quota.Value' --output text) || return 1
  q=${q%.*}
  [[ "$q" =~ ^[0-9]+$ ]] || return 1
  echo "$q"
}
reserve_of() { if [ "$1" = spot ]; then echo $WALL_RESERVE_spot; else echo $WALL_RESERVE_ondemand; fi; }
# room_for MARKET VCPUS: 0 when that many fit and still leave the wall's reserve.
room_for() {
  local quota usage ledger used reserve
  reserve=$(reserve_of "$1")
  quota=$(quota_of "$1") && usage=$(g_usage "$1") && ledger=$(ledger_recent) \
    || { say "no $1 ${GA_WHAT:-G instance}: the G quota, its usage or the admission ledger could not be read"; return 1; }
  # The listing, then the launches admitted just now that it does not show yet.
  used=$( { awk '{print $2, $4, "listed"}' <<<"$usage"; awk -v m="$1" '$3 == m {print $2, $4, "ledger"}' <<<"$ledger"; } \
    | awk '$3 == "ledger" {if (!($1 in seen)) n += $2; next} NF == 3 {seen[$1] = 1; n += $2} END {print n + 0}')
  if [ $((used + $2 + reserve)) -le "$quota" ]; then return 0; fi
  say "no $1 ${GA_WHAT:-G instance}: $used of $quota G vCPUs in use, and $2 more would leave the wall's host less than its $reserve"
  return 1
}
# Admission is one `up` at a time across every machine: a lock object in the asset bucket,
# created only if absent and deleted only by its own ETag (S3 conditional writes are strongly
# consistent), and a ledger of the boxes admitted, each counted until describe-instances
# (eventually consistent) lists it, or for ADMIT_LEDGER_S. A lock older than ADMIT_STALE_S is
# a holder that died; the holder checks the lock is still its own before it launches.
ADMIT_LOCK=gpubox/admission.lock
ADMIT_LEDGER=gpubox/admitted
ADMIT_LEDGER_S=3600
ADMIT_STALE_S=${GPUBOX_ADMIT_STALE_S:-300}
ADMIT_ETAG=""
ADMIT_AT=0
S3A() { aws --region "$S3REGION" "$@"; }
admit_lock() {
  local deadline=$(( $(now) + ${GPUBOX_ADMIT_WAIT_S:-600} )) f held etag at
  f=$(mktemp)
  while :; do
    ADMIT_AT=$(now); echo "$ADMIT_AT ${owner:-agent}" > "$f"
    if ADMIT_ETAG=$(S3A s3api put-object --bucket "$BUCKET" --key "$ADMIT_LOCK" --body "$f" --if-none-match '*' \
        --query ETag --output text 2>"$f.err"); then
      rm -f "$f" "$f.err"; return 0
    fi
    grep -q -E 'PreconditionFailed|ConditionalRequestConflict' "$f.err" || { cat "$f.err" >&2; rm -f "$f" "$f.err"; return 1; }
    if held=$(S3A s3api get-object --bucket "$BUCKET" --key "$ADMIT_LOCK" "$f.held" --query ETag --output text 2>"$f.err"); then
      etag=$held; at=$(awk 'NR == 1 {print $1}' "$f.held")
      if [[ "$at" =~ ^[0-9]+$ ]] && [ $(( $(now) - at )) -gt "$ADMIT_STALE_S" ]; then
        say "breaking an admission lock held $(( $(now) - at )) s"
        # Only that lock: a contender that broke it first and took a new one keeps its own.
        S3A s3api delete-object --bucket "$BUCKET" --key "$ADMIT_LOCK" --if-match "$etag" >/dev/null 2>&1 && continue
      fi
    elif ! grep -q NoSuchKey "$f.err"; then
      cat "$f.err" >&2; rm -f "$f" "$f.err" "$f.held"; return 1
    fi
    [ "$(now)" -lt "$deadline" ] || { rm -f "$f" "$f.err" "$f.held"; say "another launch held the admission lock"; return 1; }
    sleep 2
  done
}
# Whether the lock is still the one this `up` took, and young enough that nobody can break it
# as stale before the launch is in the ledger (half of ADMIT_STALE_S, about 150 s of margin).
admit_held() {
  local e
  [ $(( $(now) - ADMIT_AT )) -lt $(( ADMIT_STALE_S / 2 )) ] || return 1
  e=$(S3A s3api head-object --bucket "$BUCKET" --key "$ADMIT_LOCK" --query ETag --output text 2>/dev/null) || return 1
  [ -n "$ADMIT_ETAG" ] && [ "$e" = "$ADMIT_ETAG" ]
}
# A launch that may have happened and is in no ledger (run-instances failed with an unknown
# error, or the ledger could not be written and the instance could not be terminated): keep the
# lock until it is stale, by when describe-instances lists the instance, instead of releasing it.
admit_abandon() { ADMIT_ETAG=""; }
admit_unlock() {
  [ -z "$ADMIT_ETAG" ] || S3A s3api delete-object --bucket "$BUCKET" --key "$ADMIT_LOCK" --if-match "$ADMIT_ETAG" >/dev/null 2>&1 || true
  ADMIT_ETAG=""
}
# The ledger's entries of the last ADMIT_LEDGER_S seconds: "<unix time> <instance id> <market>".
# Fails when the ledger exists and cannot be read.
ledger_recent() {
  local t f; t=$(now); f=$(mktemp)
  if ! S3A s3api get-object --bucket "$BUCKET" --key "$ADMIT_LEDGER" "$f" >/dev/null 2>"$f.err"; then
    grep -q NoSuchKey "$f.err" || { cat "$f.err" >&2; rm -f "$f" "$f.err"; return 1; }
    : > "$f"
  fi
  # "<unix time> <instance id> <market> <vCPUs>"; an entry written before the vCPUs were kept is 96.
  awk -v t="$t" -v keep=$ADMIT_LEDGER_S '(NF == 3 || NF == 4) && t - $1 <= keep {print $1, $2, $3, (NF == 4 ? $4 : 96)}' "$f" || { rm -f "$f" "$f.err"; return 1; }
  rm -f "$f" "$f.err"
}
ledger_add() { # id market vcpus
  local recent
  recent=$(ledger_recent) || return 1
  { [ -z "$recent" ] || echo "$recent"; echo "$(now) $1 $2 $3"; } | S3A s3 cp --only-show-errors - "s3://$BUCKET/$ADMIT_LEDGER"
}

