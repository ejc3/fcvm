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
  # A host builder gets an IPv6 prefix, as a fleet host does: build.sh's root warm-up
  # (warm-root.sh) launches a routed VM at an address in it.
  net="DeviceIndex=0,SubnetId=$(subnet),Groups=$(sg),AssociatePublicIpAddress=true,DeleteOnTermination=true"
  [ "$kind" = host ] && net="$net,Ipv6PrefixCount=1"
  id=$(A ec2 run-instances --image-id "${BASE_AMI:-$(ubuntu_ami)}" --instance-type "$type" \
    --network-interfaces "$net" --iam-instance-profile Name=$profile \
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
  # The phase clears its own markers too, but only once it runs; until then `wait` would
  # read the previous run's (a repeated phase, or a builder from a BASE_AMI).
  ssm_sh "$2" "rm -f /var/lib/fleet-ami/$4.done /var/lib/fleet-ami/$4.failed; cd /opt/fleet-ami && nohup setsid bash $3 $4 > /dev/null 2>&1 < /dev/null & echo started $4"
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
  # Publish only the guard actually installed on this builder, checked against
  # this recipe's bundled implementation. SSM failure exits before CreateImage.
  guard_sha=$(sha256sum "$HERE/common/cve-2026-98432-mitigation.py")
  guard_sha=${guard_sha%% *}
  [[ "$guard_sha" =~ ^[0-9a-f]{64}$ ]] || { echo "invalid guard digest" >&2; exit 1; }
  ssm_sh "$2" "python3 - '$guard_sha' <<'FLEET_GUARD_PROOF'
# BEGIN IMAGE GUARD PROOF
import ast
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys

def need(value, message):
    if not value:
        raise SystemExit(message)

def output(arguments):
    return subprocess.check_output(arguments, text=True, stderr=subprocess.DEVNULL, timeout=60)

guard = Path('/usr/local/sbin/cve-2026-98432-mitigation')
service = 'cve-2026-98432-mitigation.service'
need(guard.is_file() and not guard.is_symlink(), 'installed guard is missing or unsafe')
implementation = guard.read_bytes()
need(hashlib.sha256(implementation).hexdigest() == sys.argv[1], 'installed guard digest differs')
units = [ast.literal_eval(node.value) for node in ast.parse(implementation).body if isinstance(node, ast.Assign)
         and any(isinstance(target, ast.Name) and target.id == 'UNIT' for target in node.targets)]
fragment = Path('/etc/systemd/system/' + service)
need(len(units) == 1 and isinstance(units[0], str) and fragment.is_file() and not fragment.is_symlink()
     and fragment.read_bytes() == units[0].encode(), 'guard boot unit differs from verified implementation')
proof = json.loads(output([str(guard), '--check']))
need(proof.get('status') == 'PASS' and all(proof.get(key) is True for key in
     ('inet_prerouting', 'inet_output', 'bridge_prerouting')), 'runtime guard check failed')
for family, hooks in (('inet', ('prerouting', 'output')), ('bridge', ('prerouting',))):
    items = json.loads(output(['nft', '-j', 'list', 'table', family, 'cve_2026_98432']))['nftables']
    chains = [item['chain'] for item in items if 'chain' in item]
    rules = [item['rule'] for item in items if 'rule' in item]
    need(len(chains) == len(rules) == len(hooks), 'unexpected guard chains or rules')
    protocol = {'meta': {'key': 'l4proto'}} if family == 'inet' else {'payload': {'protocol': 'ip', 'field': 'protocol'}}
    expected = [{'match': {'op': '==', 'left': protocol, 'right': 'tcp'}},
                {'match': {'op': '>', 'left': {'payload': {'protocol': 'ip', 'field': 'hdrlength'}}, 'right': 5}},
                {'drop': None}]
    for hook in hooks:
        definition = next((chain for chain in chains if chain.get('name') == hook), {})
        need(all(definition.get(key) == value for key, value in
             (('family', family), ('table', 'cve_2026_98432'), ('type', 'filter'),
              ('hook', hook), ('prio', -310), ('policy', 'accept'))), 'guard hook is ineffective')
        matching = [rule for rule in rules if rule.get('chain') == hook]
        need(len(matching) == 1 and matching[0].get('family') == family
             and matching[0].get('table') == 'cve_2026_98432', 'guard rule is missing or ambiguous')
        # nft omits redundant family matches: the IPv4 payload itself scopes this
        # rule to IPv4. Counters may change, but no additional restriction is valid.
        actual = [entry for entry in matching[0]['expr'] if set(entry) != {'counter'}]
        need(actual == expected, 'guard packet predicates differ')
need(output(['systemctl', 'is-enabled', service]).strip() == 'enabled', 'guard boot unit is not enabled')
settings = dict(line.split('=', 1) for line in output(['systemctl', 'show', service,
    '--property=ExecStart', '--property=DropInPaths', '--property=FragmentPath',
    '--property=NeedDaemonReload']).splitlines() if '=' in line)
need(all(settings.get(key) == value for key, value in (('DropInPaths', ''),
     ('FragmentPath', '/etc/systemd/system/' + service), ('NeedDaemonReload', 'no'))), 'guard boot unit is overridden')
command = r'\{ path=' + re.escape(str(guard)) + r' ; argv\[\]=' + re.escape(str(guard))
need(re.fullmatch(command + r' --load ; ignore_errors=no ; [^{}]* \}', settings.get('ExecStart', '')),
     'guard boot command differs')
selected = ()
for units in (('fcvm-fleet-agent.service', 'fcvm-fleet-store.service'),
              ('fcvm-fleet-renderer.service', 'fcvm-fleet-nginx.service')):
    states = [output(['systemctl', 'show', unit, '--property=LoadState', '--value']).strip() for unit in units]
    if 'loaded' in states:
        need(all(state == 'loaded' for state in states) and not selected, 'incomplete or ambiguous fleet services')
        selected = units
need(selected, 'no fleet services installed')
for unit in selected:
    settings = dict(line.split('=', 1) for line in output([
        'systemctl', 'show', unit, '--property=Requires', '--property=After']).splitlines() if '=' in line)
    need(all(service in settings.get(key, '').split() for key in ('Requires', 'After')),
         'fleet service lacks mandatory guard ordering')
print('fleet image guard publication proof passed')
# END IMAGE GUARD PROOF
FLEET_GUARD_PROOF"
  ami=$(A ec2 create-image --instance-id "$2" --name "$3" --description "fcvm fleet: $3" \
    --tag-specifications "ResourceType=image,Tags=[{Key=Name,Value=$3},{Key=Purpose,Value=fcvm-fleet},{Key=GuardHelperSHA256,Value=$guard_sha}]" \
      "ResourceType=snapshot,Tags=[{Key=Name,Value=$3},{Key=Purpose,Value=fcvm-fleet},{Key=GuardHelperSHA256,Value=$guard_sha}]" \
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
