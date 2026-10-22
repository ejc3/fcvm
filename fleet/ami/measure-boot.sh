#!/bin/bash
# Measure a fleet AMI from the launch request, as the coordinator would see it.
#
#   measure-boot.sh tile <ami>   g4dn.xlarge spot, launched as the tile template launches it; polls
#                                https://<its address>/health from here until the renderer serves a
#                                frame (EC2's best case in the race: launch request to first frame)
#   measure-boot.sh host <ami>   g4dn.metal; times until the GPUs are on vfio-pci and the store is
#                                restored (the host is ready for the agent), then starts one GPU VM
#                                and times it to first frame (fleet/ami/host/warm.sh)
#
# The instance is terminated at the end. Fleet-account credentials in the environment.
set -euo pipefail
KIND=$1; AMI=$2
REGION=${REGION:-us-east-1}
HERE=$(cd "$(dirname "$0")" && pwd)
A() { aws --region "$REGION" "$@"; }
now() { date +%s%3N; }
subnet=$(A ec2 describe-subnets --filters Name=tag:Name,Values="fcvm-fleet-${REGION}${AZ_SUFFIX:-a}" --query 'Subnets[0].SubnetId' --output text)
sg=$(A ec2 describe-security-groups --filters Name=group-name,Values=fcvm-fleet --query 'SecurityGroups[0].GroupId' --output text)
case "$KIND" in
  tile) type=g4dn.xlarge; profile=fcvm-fleet-tile; net="Ipv6AddressCount=1"
        market=(--instance-market-options 'MarketType=spot,SpotOptions={SpotInstanceType=one-time,InstanceInterruptionBehavior=terminate}') ;;
  host) type=g4dn.metal; profile=fcvm-fleet-host; net="Ipv6PrefixCount=1"; market=() ;;
  *) echo "measure-boot.sh tile|host <ami>" >&2; exit 2 ;;
esac
t0=$(now)
id=$(A ec2 run-instances --image-id "$AMI" --instance-type $type "${market[@]}" \
  --network-interfaces "DeviceIndex=0,SubnetId=$subnet,Groups=$sg,AssociatePublicIpAddress=true,$net,DeleteOnTermination=true" \
  --iam-instance-profile Name=$profile \
  --metadata-options HttpTokens=required,HttpPutResponseHopLimit=1,HttpEndpoint=enabled \
  --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=fcvm-fleet-ami-measure-$KIND},{Key=Purpose,Value=fcvm-fleet-ami-build}]" \
  --query 'Instances[0].InstanceId' --output text)
t_accepted=$(now)
trap 'A ec2 terminate-instances --instance-ids "$id" >/dev/null; echo "terminated $id"' EXIT
echo "launched $id: accepted after $((t_accepted - t0)) ms"
ip=""
while [ -z "$ip" ] || [ "$ip" = None ]; do sleep 1; ip=$(A ec2 describe-instances --instance-ids "$id" --query 'Reservations[0].Instances[0].PublicIpAddress' --output text); done
t_running=""
if [ "$KIND" = tile ]; then
  while :; do
    [ -z "$t_running" ] && [ "$(A ec2 describe-instances --instance-ids "$id" --query 'Reservations[0].Instances[0].State.Name' --output text)" = running ] && t_running=$(now)
    if curl -fsS -m 2 --resolve "tile.fcvm.dev:443:$ip" https://tile.fcvm.dev/health 2>/dev/null | jq -e '.ready == true and .frame > 0' >/dev/null 2>&1; then break; fi
    [ $(( $(now) - t0 )) -lt 900000 ] || { echo "no frame in 15 minutes"; exit 1; }
    sleep 0.5
  done
  t_frame=$(now)
  echo "tile: request->accepted $((t_accepted - t0)) ms, ->running (seen) $((t_running - t0)) ms, ->first frame over HTTPS $((t_frame - t0)) ms"
fi
ssm() {
  local cid st
  cid=$(A ssm send-command --instance-ids "$id" --document-name AWS-RunShellScript --parameters "$(jq -nc --arg c "$1" '{commands:[$c],executionTimeout:["1800"]}')" --query Command.CommandId --output text)
  while :; do st=$(A ssm get-command-invocation --command-id "$cid" --instance-id "$id" --query Status --output text 2>/dev/null || echo Pending)
    case "$st" in Pending|InProgress|Delayed) sleep 2 ;; *) break ;; esac; done
  A ssm get-command-invocation --command-id "$cid" --instance-id "$id" --query StandardOutputContent --output text
}
until [ "$(A ssm describe-instance-information --filters Key=InstanceIds,Values="$id" --query 'InstanceInformationList[0].PingStatus' --output text 2>/dev/null)" = Online ]; do sleep 5; done
t_ssm=$(now)
launch=$(A ec2 describe-instances --instance-ids "$id" --query 'Reservations[0].Instances[0].LaunchTime' --output text)
echo "ssm online after $((t_ssm - t0)) ms; EC2 LaunchTime $launch"
units="fcvm-fleet-vfio fcvm-fleet-store"; [ "$KIND" = tile ] && units="nvidia-persistenced fcvm-fleet-tls fcvm-fleet-renderer nginx"
ssm "echo kernel_start=\$(( \$(date +%s%3N) - \$(awk '{printf \"%d\", \$1*1000}' /proc/uptime) )); for u in $units; do echo \$u \$(systemctl show -p ActiveEnterTimestamp --value \$u.service | xargs -I{} date -d '{}' +%s%3N 2>/dev/null) \$(systemctl show -p ActiveState --value \$u.service); done; cat /var/log/fcvm-fleet-first-frame.log 2>/dev/null; journalctl -u fcvm-fleet-store -o cat --no-pager 2>/dev/null | grep -E 'fcvm-fleet-store:' ; cat /run/fcvm-fleet/gpus 2>/dev/null | wc -l" | while read -r a b c; do
  case "$a" in
    kernel_start=*) echo "kernel started $(( ${a#kernel_start=} - t0 )) ms after the request" ;;
    first_frame) echo "on-instance: $a $b $c" ;;
    fcvm-fleet-store:*) echo "on-instance: $a $b $c" ;;
    *) [ -n "$b" ] && [ "$b" -gt 0 ] 2>/dev/null && echo "$a active ($c) $(( b - t0 )) ms after the request" || echo "$a $b $c" ;;
  esac
done
if [ "$KIND" = host ]; then
  ssm "G0=\$(ls /sys/bus/pci/drivers/vfio-pci | grep -E '^[0-9a-f]{4}:' | head -1); prlimit --memlock=unlimited:unlimited sudo -u ubuntu -H bash -c \"cd /tmp && bash /usr/local/lib/fcvm-fleet/warm.sh \$G0 /tmp/warm.log\" 2>&1 | tail -3; grep -o '\"status\":\"restored\"[^}]*' /tmp/warm.log | head -1"
  t_vm=$(now)
  echo "host: first GPU VM done $((t_vm - t0)) ms after the request (includes SSM round trips)"
fi
