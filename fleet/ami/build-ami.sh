#!/bin/bash
# Build the fcvm fleet AMIs in the fcvm-fleet account (design: docs/fleet/ARCHITECTURE.md,
# "Hosts and capacity" and "EC2 tiles"). Runs where the account's Terraform role can be
# assumed, with its credentials in the environment. The builders have no SSH (the fleet's
# security group admits TCP 443 only): everything goes through SSM Run Command, and files
# reach them through presigned S3 URLs, since Ubuntu's images carry no AWS CLI.
#
#   build-ami.sh launch host|tile           launch a builder, print its instance id
#   build-ami.sh push   <id>                copy fleet/ami to /opt/fleet-ami on the builder
#   build-ami.sh env    <id> K=V...         write /var/lib/fleet-ami/env (read by the phases)
#   build-ami.sh start  <id> <script> <phase>   run a phase detached (logs in /var/log/fleet-ami)
#   build-ami.sh wait   <id> <phase>        wait for <phase>.done or .failed, print the log tail
#   build-ami.sh sh     <id> <command>      run a command and print its output
#   build-ami.sh reboot <id>                reboot and wait until SSM answers from the new boot
#   build-ami.sh image  <id> <name>         create the AMI (instance reboots), wait until available
#   build-ami.sh put-url <key>              presigned PUT URL for s3://<assets>/<key> (6 h)
#   build-ami.sh get-url <key>              presigned GET URL for s3://<assets>/<key> (6 h)
#
# Environment: REGION (default us-east-1), SUBNET (default: the fleet subnet in us-east-1a),
# BASE_AMI (launch from an earlier fleet AMI instead of Ubuntu 24.04, to revise it).
set -euo pipefail
REGION=${REGION:-us-east-1}
HERE=$(cd "$(dirname "$0")" && pwd)
ACCOUNT=$(aws sts get-caller-identity --query Account --output text)
BUCKET=fcvm-fleet-assets-$ACCOUNT
A() { aws --region "$REGION" "$@"; }
S3REGION=${S3REGION:-us-east-2}   # the asset bucket is in the home region
S3() { aws --region "$S3REGION" "$@"; }

ubuntu_ami() { A ssm get-parameter --name /aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id --query Parameter.Value --output text; }
subnet() {
  [ -n "${SUBNET:-}" ] && { echo "$SUBNET"; return; }
  A ec2 describe-subnets --filters Name=tag:Name,Values="fcvm-fleet-${REGION}${AZ_SUFFIX:-a}" --query 'Subnets[0].SubnetId' --output text
}
sg() { A ec2 describe-security-groups --filters Name=group-name,Values=fcvm-fleet --query 'SecurityGroups[0].GroupId' --output text; }

ssm_send() { # id, command -> command id
  local params; params=$(jq -nc --arg c "$2" '{commands: [$c], executionTimeout: ["7200"]}')
  A ssm send-command --instance-ids "$1" --document-name AWS-RunShellScript --parameters "$params" \
    --timeout-seconds 600 --query Command.CommandId --output text
}
ssm_sh() { # id, command: wait and print stdout and stderr; exit with its status
  local cid st; cid=$(ssm_send "$1" "$2")
  while :; do
    st=$(A ssm get-command-invocation --command-id "$cid" --instance-id "$1" --query Status --output text 2>/dev/null || echo Pending)
    case "$st" in Pending|InProgress|Delayed) sleep 3 ;; *) break ;; esac
  done
  A ssm get-command-invocation --command-id "$cid" --instance-id "$1" --query '[StandardOutputContent,StandardErrorContent]' --output text
  [ "$st" = Success ]
}
wait_ssm() {
  for _ in $(seq 1 120); do
    [ "$(A ssm describe-instance-information --filters Key=InstanceIds,Values="$1" --query 'InstanceInformationList[0].PingStatus' --output text 2>/dev/null)" = Online ] && return 0
    sleep 10
  done
  echo "SSM never came online on $1" >&2; return 1
}

case "$1" in
launch)
  kind=$2
  case "$kind" in
    host) type=${TYPE:-g4dn.metal}; profile=fcvm-fleet-host; size=64 ;;
    tile) type=${TYPE:-g4dn.xlarge}; profile=fcvm-fleet-tile; size=32 ;;
    *) echo "launch host|tile" >&2; exit 2 ;;
  esac
  id=$(A ec2 run-instances --image-id "${BASE_AMI:-$(ubuntu_ami)}" --instance-type "$type" --subnet-id "$(subnet)" \
    --security-group-ids "$(sg)" --iam-instance-profile Name=$profile \
    --metadata-options HttpTokens=required,HttpPutResponseHopLimit=1,HttpEndpoint=enabled \
    --block-device-mappings "[{\"DeviceName\":\"/dev/sda1\",\"Ebs\":{\"VolumeSize\":$size,\"VolumeType\":\"gp3\",\"DeleteOnTermination\":true}}]" \
    --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=fcvm-fleet-ami-builder-$kind},{Key=Purpose,Value=fcvm-fleet-ami-build}]" \
    --query 'Instances[0].InstanceId' --output text)
  echo "$id"
  A ec2 wait instance-running --instance-ids "$id" >&2
  wait_ssm "$id" >&2
  ;;
push)
  key=ami-build/$(date -u +%Y%m%dT%H%M%S)-fleet-ami.tgz
  tar -C "$HERE/.." -czf - ami | S3 s3 cp - "s3://$BUCKET/$key" --only-show-errors
  url=$(S3 s3 presign "s3://$BUCKET/$key" --expires-in 3600)
  ssm_sh "$2" "rm -rf /opt/fleet-ami && mkdir -p /opt && curl -fsS '$url' | tar -xz -C /opt && mv /opt/ami /opt/fleet-ami && ls /opt/fleet-ami"
  ;;
env)
  id=$2; shift 2
  body=$(printf '%s\n' "$@" | sed "s/'/'\\\\''/g; s/^\([A-Z_]*\)=\(.*\)$/\1='\2'/")
  ssm_sh "$id" "mkdir -p /var/lib/fleet-ami && umask 077 && cat > /var/lib/fleet-ami/env <<'XEOF'
$body
XEOF
cut -d= -f1 /var/lib/fleet-ami/env"
  ;;
start)
  ssm_sh "$2" "cd /opt/fleet-ami && nohup setsid bash $3 $4 > /dev/null 2>&1 < /dev/null & echo started $4"
  ;;
wait)
  id=$2; ph=$3
  while :; do
    st=$(ssm_sh "$id" "cd /var/lib/fleet-ami; if [ -e $ph.done ]; then echo done; elif [ -e $ph.failed ]; then echo failed; else echo running; fi" 2>/dev/null | head -1 || echo unknown)
    case "$st" in "done"|"failed") break ;; esac
    sleep 30
  done
  ssm_sh "$id" "tail -40 /var/log/fleet-ami/$ph.log" || true
  echo "phase $ph: $st"
  [ "$st" = "done" ]
  ;;
sh) ssm_sh "$2" "$3" ;;
reboot)
  old=$(ssm_sh "$2" "cat /proc/sys/kernel/random/boot_id" | head -1)
  A ec2 reboot-instances --instance-ids "$2"
  sleep 60
  for _ in $(seq 1 60); do
    new=$(ssm_sh "$2" "cat /proc/sys/kernel/random/boot_id" 2>/dev/null | head -1 || true)
    [ -n "$new" ] && [ "$new" != "$old" ] && { echo "rebooted"; exit 0; }
    sleep 15
  done
  echo "no new boot on $2" >&2; exit 1
  ;;
image)
  ami=$(A ec2 create-image --instance-id "$2" --name "$3" --description "fcvm fleet: $3" \
    --tag-specifications "ResourceType=image,Tags=[{Key=Name,Value=$3},{Key=Purpose,Value=fcvm-fleet}]" \
      "ResourceType=snapshot,Tags=[{Key=Name,Value=$3},{Key=Purpose,Value=fcvm-fleet}]" \
    --query ImageId --output text)
  echo "$ami"
  for _ in $(seq 1 120); do
    st=$(A ec2 describe-images --image-ids "$ami" --query 'Images[0].State' --output text)
    [ "$st" = available ] && exit 0
    [ "$st" = failed ] && { echo "image $ami failed" >&2; exit 1; }
    sleep 30
  done
  echo "image $ami not available after an hour" >&2; exit 1
  ;;
put-url)
  python3 -c 'import boto3,sys; print(boto3.client("s3",region_name=sys.argv[1]).generate_presigned_url("put_object",Params={"Bucket":sys.argv[2],"Key":sys.argv[3]},ExpiresIn=21600))' "$S3REGION" "$BUCKET" "$2"
  ;;
get-url) S3 s3 presign "s3://$BUCKET/$2" --expires-in 21600 ;;
*) sed -n 2,22p "$0"; exit 2 ;;
esac
