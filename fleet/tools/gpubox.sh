#!/bin/bash
# gpubox: a leased g4dn.metal (8 T4 on vfio-pci, fcvm built, gpu profile set up, renderer
# warmed) from the fleet's GPU host AMI, in the fcvm-fleet account, for one agent's GPU work.
#
#   gpubox.sh up [--hours N] [--owner NAME] [--no-wait]
#                                  launch (spot, then on demand), wait until ready, print
#                                  "<instance id> <region>"; progress goes to stderr
#   gpubox.sh wait <id>            wait until ready: GPUs on vfio-pci and the store restored
#   gpubox.sh run <id> [-u] <cmd>  run a shell command as root (-u: as ubuntu, the fcvm user,
#                                  with VFIO's memlock and the user's runtime dir); prints
#                                  stdout and stderr, exits with the command's status
#   gpubox.sh copy <id>:<remote> <local>   copy a file off the box (through the asset bucket)
#   gpubox.sh copy <local> <id>:<remote>   copy a file onto the box
#   gpubox.sh extend <id> <hours>  move the lease end to <hours> from now
#   gpubox.sh down <id>            terminate exactly that instance
#   gpubox.sh list                 every gpubox, with owner, lease end and state
#
# Access is SSM Run Command only: the fleet's security group admits TCP 443 and nothing
# else, and stays that way. Output of one `run` is cut by SSM at 24,000 characters; write
# large output to a file and `copy` it. A command's limit is GPUBOX_TIMEOUT seconds (3600).
#
# The lease: every box powers itself off at its lease end (a systemd timer written at first
# boot) and powering off terminates it, so a forgotten box stops costing money then. Default
# 4 hours, at most 24; `extend` moves it. Only instances tagged Purpose=fcvm-fleet-gpubox are
# acted on, so the fleet's own hosts, groups and services are never touched.
#
# Credentials: assumes fcvm-fleet-terraform (named in SSM /infra/fcvm-fleet, us-west-1) unless
# the environment already holds that role's credentials (or GPUBOX_USE_ENV=1 says to use them).
# Quota (us-east-1, 2026-10-23): G spot 192 vCPU, two g4dn.metal at once, shared with the
# fleet's own spot host; G on demand 384 vCPU, four.
set -euo pipefail
REGION=us-east-1
S3REGION=us-east-2
PURPOSE=fcvm-fleet-gpubox
TYPE=g4dn.metal
AZS=${GPUBOX_AZS:-a b c d f}

say() { echo "gpubox: $*" >&2; }
die() { say "$*"; exit 1; }
A() { aws --region "$REGION" "$@"; }
now() { date +%s; }

# Already the fleet's role (a caller that assumed it): use those credentials.
if [ -n "${AWS_SESSION_TOKEN:-}" ] && aws sts get-caller-identity --query Arn --output text 2>/dev/null | grep -q ':assumed-role/fcvm-fleet-terraform/'; then
  GPUBOX_USE_ENV=1
fi
if [ "${GPUBOX_USE_ENV:-}" != 1 ]; then
  p=$(aws ssm get-parameter --region us-west-1 --name /infra/fcvm-fleet --query Parameter.Value --output text)
  unset AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY AWS_SESSION_TOKEN
  c=$(aws sts assume-role --role-arn "$(jq -r .terraform_role <<<"$p")" --role-session-name "gpubox-${USER:-agent}" \
        --duration-seconds 3600 --query Credentials --output json)
  AWS_ACCESS_KEY_ID=$(jq -r .AccessKeyId <<<"$c")
  AWS_SECRET_ACCESS_KEY=$(jq -r .SecretAccessKey <<<"$c")
  AWS_SESSION_TOKEN=$(jq -r .SessionToken <<<"$c")
  export AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY AWS_SESSION_TOKEN
  unset p c
fi
ACCOUNT=$(aws sts get-caller-identity --query Account --output text)
BUCKET=fcvm-fleet-assets-$ACCOUNT

# The newest fleet host AMI unless GPUBOX_AMI names one.
host_ami() {
  [ -n "${GPUBOX_AMI:-}" ] && { echo "$GPUBOX_AMI"; return; }
  A ec2 describe-images --owners self --filters Name=name,Values='fcvm-fleet-host-x86_64-*' Name=state,Values=available \
    --query 'sort_by(Images,&CreationDate)[-1].ImageId' --output text
}

# Refuse anything that is not a gpubox: the fleet's hosts and tiles carry other Purpose tags.
check_box() {
  local purpose
  purpose=$(A ec2 describe-instances --instance-ids "$1" --query "Reservations[0].Instances[0].Tags[?Key=='Purpose']|[0].Value" --output text 2>/dev/null) \
    || die "no instance $1 in $REGION"
  [ "$purpose" = "$PURPOSE" ] || die "$1 is not a gpubox (Purpose=$purpose); refusing"
}

ssm() { # id, command: print output, return the command's exit status
  local cid st code
  cid=$(A ssm send-command --instance-ids "$1" --document-name AWS-RunShellScript \
        --parameters "$(jq -nc --arg c "$2" --arg t "${GPUBOX_TIMEOUT:-3600}" '{commands:[$c], executionTimeout:[$t]}')" \
        --timeout-seconds 600 --query Command.CommandId --output text)
  while :; do
    st=$(A ssm get-command-invocation --command-id "$cid" --instance-id "$1" --query Status --output text 2>/dev/null || echo Pending)
    case "$st" in Pending|InProgress|Delayed) sleep 2 ;; *) break ;; esac
  done
  A ssm get-command-invocation --command-id "$cid" --instance-id "$1" --output json \
    | jq -r '.StandardOutputContent, (.StandardErrorContent | select(. != "") | "--- stderr ---\n" + .)' | sed '${/^$/d}'
  code=$(A ssm get-command-invocation --command-id "$cid" --instance-id "$1" --query ResponseCode --output text)
  [ "$st" = Success ] || say "command $st (exit $code)"
  return "$([ "$code" -ge 0 ] 2>/dev/null && echo "$code" || echo 1)"
}

wait_ready() {
  local t0=$1 id=$2 st
  until [ "$(A ssm describe-instance-information --filters Key=InstanceIds,Values="$id" --query 'InstanceInformationList[0].PingStatus' --output text 2>/dev/null)" = Online ]; do
    [ "$(A ec2 describe-instances --instance-ids "$id" --query 'Reservations[0].Instances[0].State.Name' --output text)" = running ] \
      || [ "$(A ec2 describe-instances --instance-ids "$id" --query 'Reservations[0].Instances[0].State.Name' --output text)" = pending ] \
      || die "$id is no longer running"
    sleep 5
  done
  say "SSM online after $(( $(now) - t0 )) s"
  while :; do
    st=$(ssm "$id" "systemctl is-active fcvm-fleet-vfio.service fcvm-fleet-store.service | paste -sd' '" 2>/dev/null | head -1 || true)
    case "$st" in
      "active active") break ;;
      *failed*) ssm "$id" "journalctl -u fcvm-fleet-vfio -u fcvm-fleet-store --no-pager -o cat | tail -20" >&2 || true; die "$id: boot units failed ($st)" ;;
    esac
    sleep 10
  done
  say "ready after $(( $(now) - t0 )) s: $(ssm "$id" "wc -l < /run/fcvm-fleet/gpus") GPUs on vfio-pci, store restored"
}

lease_userdata() { # lease end, "YYYY-MM-DD HH:MM:SS"
  cat <<EOF
#!/bin/bash
# gpubox lease: power off (and so terminate) at the lease end.
cat > /etc/systemd/system/gpubox-lease.service <<'X'
[Unit]
Description=gpubox lease end: power off, which terminates the instance
[Service]
Type=oneshot
ExecStart=/usr/bin/systemctl poweroff
X
cat > /etc/systemd/system/gpubox-lease.timer <<'X'
[Unit]
Description=gpubox lease end
[Timer]
OnCalendar=$1 UTC
Persistent=true
AccuracySec=1s
[Install]
WantedBy=timers.target
X
systemctl daemon-reload
systemctl enable --now gpubox-lease.timer
EOF
}

case "${1:-}" in
up)
  shift; hours=4; owner=${GPUBOX_OWNER:-${USER:-agent}}; wait=1
  while [ $# -gt 0 ]; do
    case "$1" in
      --hours) hours=$2; shift 2 ;;
      --owner) owner=$2; shift 2 ;;
      --no-wait) wait=0; shift ;;
      *) die "unknown option $1" ;;
    esac
  done
  [[ "$hours" =~ ^[0-9]+$ ]] && [ "$hours" -ge 1 ] && [ "$hours" -le 24 ] || die "--hours must be 1 to 24"
  lease=$(date -u -d "+$hours hours" '+%Y-%m-%d %H:%M:%S')
  ami=$(host_ami); [ "$ami" != None ] || die "no fleet host AMI in $REGION"
  sg=$(A ec2 describe-security-groups --filters Name=group-name,Values=fcvm-fleet --query 'SecurityGroups[0].GroupId' --output text)
  ud=$(lease_userdata "$lease")
  t0=$(now); id=""
  for market in spot on-demand; do
    for az in $AZS; do
      subnet=$(A ec2 describe-subnets --filters Name=tag:Name,Values="fcvm-fleet-${REGION}${az}" --query 'Subnets[0].SubnetId' --output text)
      mo=(); [ "$market" = spot ] && mo=(--instance-market-options 'MarketType=spot,SpotOptions={SpotInstanceType=one-time,InstanceInterruptionBehavior=terminate}')
      if id=$(A ec2 run-instances --image-id "$ami" --instance-type $TYPE "${mo[@]}" \
          --instance-initiated-shutdown-behavior terminate \
          --network-interfaces "DeviceIndex=0,SubnetId=$subnet,Groups=$sg,AssociatePublicIpAddress=true,Ipv6PrefixCount=1,DeleteOnTermination=true" \
          --iam-instance-profile Name=fcvm-fleet-host \
          --metadata-options HttpTokens=required,HttpPutResponseHopLimit=1,HttpEndpoint=enabled \
          --user-data "$ud" \
          --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=gpubox-$owner},{Key=Purpose,Value=$PURPOSE},{Key=Owner,Value=$owner},{Key=LeaseUntil,Value=${lease/ /T}Z},{Key=Market,Value=$market}]" \
          --query 'Instances[0].InstanceId' --output text 2>/tmp/gpubox.err.$$); then
        break 2
      fi
      grep -q -E 'InsufficientInstanceCapacity|MaxSpotInstanceCountExceeded|VcpuLimitExceeded|SpotMaxPriceTooLow|Unsupported' /tmp/gpubox.err.$$ \
        || { cat /tmp/gpubox.err.$$ >&2; rm -f /tmp/gpubox.err.$$; exit 1; }
      say "no $market $TYPE in ${REGION}${az}: $(grep -o -E '\(([A-Za-z]+)\)' /tmp/gpubox.err.$$ | head -1)"
      id=""
    done
  done
  rm -f /tmp/gpubox.err.$$
  [ -n "$id" ] || die "no $TYPE capacity or quota in $REGION, spot or on demand"
  say "launched $id ($market, ${REGION}${az}, $ami), lease until $lease UTC"
  [ "$wait" = 0 ] || wait_ready "$t0" "$id"
  echo "$id $REGION"
  ;;
wait) check_box "$2"; wait_ready "$(now)" "$2" ;;
run)
  id=$2; shift 2; check_box "$id"
  if [ "${1:-}" = -u ]; then
    shift
    q=$(printf '%q' "$*")
    ssm "$id" "prlimit --memlock=unlimited:unlimited sudo -u ubuntu -H XDG_RUNTIME_DIR=/run/user/\$(id -u ubuntu) bash -lc $q"
  else
    ssm "$id" "$*"
  fi
  ;;
copy)
  src=$2; dst=$3; key=gpubox/$(date +%s)-$$-$RANDOM
  if [[ "$src" == i-*:* ]]; then
    id=${src%%:*}; remote=${src#*:}; check_box "$id"
    put=$(python3 -c 'import boto3,sys; print(boto3.client("s3",region_name=sys.argv[1]).generate_presigned_url("put_object",Params={"Bucket":sys.argv[2],"Key":sys.argv[3]},ExpiresIn=3000))' "$S3REGION" "$BUCKET" "$key")
    ssm "$id" "curl -fsS --retry 3 -X PUT --upload-file $(printf '%q' "$remote") '$put'" >/dev/null || die "upload from $id failed"
    aws --region "$S3REGION" s3 cp --only-show-errors "s3://$BUCKET/$key" "$dst"
  elif [[ "$dst" == i-*:* ]]; then
    id=${dst%%:*}; remote=${dst#*:}; check_box "$id"
    aws --region "$S3REGION" s3 cp --only-show-errors "$src" "s3://$BUCKET/$key"
    get=$(aws --region "$S3REGION" s3 presign "s3://$BUCKET/$key" --expires-in 3000)
    ssm "$id" "curl -fsS --retry 3 -o $(printf '%q' "$remote") '$get'" >/dev/null || { aws --region "$S3REGION" s3 rm --only-show-errors "s3://$BUCKET/$key"; die "download to $id failed"; }
  else
    die "copy <id>:<remote> <local> | copy <local> <id>:<remote>"
  fi
  aws --region "$S3REGION" s3 rm --only-show-errors "s3://$BUCKET/$key"
  ;;
extend)
  id=$2; hours=$3; check_box "$id"
  [[ "$hours" =~ ^[0-9]+$ ]] && [ "$hours" -ge 1 ] && [ "$hours" -le 24 ] || die "hours must be 1 to 24"
  lease=$(date -u -d "+$hours hours" '+%Y-%m-%d %H:%M:%S')
  ssm "$id" "sed -i 's/^OnCalendar=.*/OnCalendar=$lease UTC/' /etc/systemd/system/gpubox-lease.timer && systemctl daemon-reload && systemctl restart gpubox-lease.timer && systemctl list-timers gpubox-lease.timer --no-pager | sed -n 2p" >&2
  A ec2 create-tags --resources "$id" --tags "Key=LeaseUntil,Value=${lease/ /T}Z"
  say "lease until $lease UTC"
  ;;
down)
  id=$2; check_box "$id"
  A ec2 terminate-instances --instance-ids "$id" --query 'TerminatingInstances[0].CurrentState.Name' --output text >&2
  ;;
list)
  A ec2 describe-instances --filters Name=tag:Purpose,Values=$PURPOSE Name=instance-state-name,Values=pending,running,stopping,shutting-down \
    --query "Reservations[].Instances[].[InstanceId,State.Name,Tags[?Key=='Owner']|[0].Value,Tags[?Key=='LeaseUntil']|[0].Value,Tags[?Key=='Market']|[0].Value,LaunchTime]" --output text
  ;;
*) sed -n 2,33p "$0"; exit 2 ;;
esac
