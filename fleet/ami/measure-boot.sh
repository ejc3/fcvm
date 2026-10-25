#!/bin/bash
# Measure a fleet AMI from the launch request, as the coordinator would see it.
#
#   measure-boot.sh tile <ami>   g4dn.xlarge spot, launched as the tile template launches it; polls
#                                https://<its address>/health from here until the renderer serves a
#                                frame (EC2's best case in the race: launch request to first frame)
#   measure-boot.sh host <ami>   g4dn.metal; times until the GPUs are on vfio-pci and the store is
#                                restored (the host is ready for the agent), then starts one GPU VM
#                                and times it to first frame (fleet/ami/host/warm.sh), printing
#                                fcvm's restored record and whether it hit the build's snapshot
#
# The instance is terminated at the end. Fleet-account credentials in the environment.
set -euo pipefail
KIND=$1; AMI=$2
REGION=${REGION:-us-east-1}
A() { aws --region "$REGION" "$@"; }
now() { date +%s%3N; }
sg=$(A ec2 describe-security-groups --filters Name=group-name,Values=fcvm-fleet --query 'SecurityGroups[0].GroupId' --output text)
case "$KIND" in
  tile) type=g4dn.xlarge; profile=fcvm-fleet-tile; net="Ipv6AddressCount=1"
        market=(--instance-market-options 'MarketType=spot,SpotOptions={SpotInstanceType=one-time,InstanceInterruptionBehavior=terminate}') ;;
  host) type=g4dn.metal; profile=fcvm-fleet-host; net="Ipv6PrefixCount=1"; market=() ;;
  *) echo "measure-boot.sh tile|host <ami>" >&2; exit 2 ;;
esac
# MARKET=on-demand measures a tile when spot has no capacity: the boot does not depend on it.
[ "${MARKET:-}" = on-demand ] && market=()
# Like the coordinator, an AZ that has no capacity is skipped; the time spent on refusals
# is reported apart (capacity wait) and the clock starts at the request that was accepted.
tw=$(now); id=""
for az in ${AZS:-a b c d f}; do
  subnet=$(A ec2 describe-subnets --filters Name=tag:Name,Values="fcvm-fleet-${REGION}${az}" --query 'Subnets[0].SubnetId' --output text)
  t0=$(now)
  id=$(A ec2 run-instances --image-id "$AMI" --instance-type $type "${market[@]}" \
    --network-interfaces "DeviceIndex=0,SubnetId=$subnet,Groups=$sg,AssociatePublicIpAddress=true,$net,DeleteOnTermination=true" \
    --iam-instance-profile Name=$profile \
    --metadata-options HttpTokens=required,HttpPutResponseHopLimit=1,HttpEndpoint=enabled \
    --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=fcvm-fleet-ami-measure-$KIND},{Key=Purpose,Value=fcvm-fleet-ami-build}]" \
    --query 'Instances[0].InstanceId' --output text 2>/tmp/measure-boot.err.$$) && break
  grep -q -E 'InsufficientInstanceCapacity|capacity' /tmp/measure-boot.err.$$ || { cat /tmp/measure-boot.err.$$ >&2; exit 1; }
  echo "no $type capacity in ${REGION}${az}"
done
rm -f /tmp/measure-boot.err.$$
[ -n "$id" ] || { echo "no capacity in any AZ"; exit 1; }
echo "capacity wait $((t0 - tw)) ms (not counted below), AZ ${REGION}${az}"
t_accepted=$(now)
trap 'A ec2 terminate-instances --instance-ids "$id" >/dev/null; echo "terminated $id"' EXIT
echo "launched $id: accepted after $((t_accepted - t0)) ms"
ip=""
while [ -z "$ip" ] || [ "$ip" = None ]; do sleep 1; ip=$(A ec2 describe-instances --instance-ids "$id" --query 'Reservations[0].Instances[0].PublicIpAddress' --output text); done
t_running=""
if [ "$KIND" = tile ]; then
  while :; do
    [ -z "$t_running" ] && [ "$(A ec2 describe-instances --instance-ids "$id" --query 'Reservations[0].Instances[0].State.Name' --output text)" = running ] && t_running=$(now)
    if curl -fsS -m 2 --connect-to "tile.fcvm.dev:443:$ip:443" https://tile.fcvm.dev/health 2>/dev/null | jq -e '.ready == true and .frame > 0' >/dev/null 2>&1; then break; fi
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
  # A command that failed on the instance must not be reported as a measurement.
  [ "$st" = Success ] || { echo "measure-boot: command ended $st on the instance" >&2; return 1; }
}
until [ "$(A ssm describe-instance-information --filters Key=InstanceIds,Values="$id" --query 'InstanceInformationList[0].PingStatus' --output text 2>/dev/null)" = Online ]; do sleep 5; done
t_ssm=$(now)
launch=$(A ec2 describe-instances --instance-ids "$id" --query 'Reservations[0].Instances[0].LaunchTime' --output text)
echo "ssm online after $((t_ssm - t0)) ms; EC2 LaunchTime $launch"
units="fcvm-fleet-vfio fcvm-fleet-store"; [ "$KIND" = tile ] && units="nvidia-persistenced cloud-init fcvm-fleet-tls fcvm-fleet-renderer fcvm-fleet-nginx"
# Per unit: when its start job began (InactiveExit) and when it became active, from the
# monotonic stamps (microseconds since the kernel started) added to the kernel's start, so
# the times are not rounded to the second; 0 for a unit that never started.
ssm "k=\$(( \$(date +%s%3N) - \$(awk '{printf \"%d\", \$1*1000}' /proc/uptime) )); echo kernel_start=\$k; for u in $units; do a=\$(systemctl show -p ActiveEnterTimestampMonotonic --value \$u.service); i=\$(systemctl show -p InactiveExitTimestampMonotonic --value \$u.service); echo \$u \$(( \${a:-0} > 0 ? k + \${a:-0} / 1000 : 0 )) \$(systemctl show -p ActiveState --value \$u.service) \$(( \${i:-0} > 0 ? k + \${i:-0} / 1000 : 0 )); done; cat /var/log/fcvm-fleet-first-frame.log 2>/dev/null; journalctl -u fcvm-fleet-store -o cat --no-pager 2>/dev/null | grep -E 'fcvm-fleet-store:' ; cat /run/fcvm-fleet/gpus 2>/dev/null | wc -l" | while read -r a b c d; do
  case "$a" in
    kernel_start=*) echo "kernel started $(( ${a#kernel_start=} - t0 )) ms after the request" ;;
    first_frame) echo "on-instance: $a $b $c" ;;
    fcvm-fleet-store:*) echo "on-instance: $a $b $c" ;;
    *) [ -n "$b" ] && [ "$b" -gt 0 ] 2>/dev/null && echo "$a started $(( ${d:-0} - t0 )) ms, active ($c) $(( b - t0 )) ms after the request" || echo "$a $b $c" ;;
  esac
done
if [ "$KIND" = host ]; then
  # Ready for the agent: the GPUs on vfio-pci and the store restored from the AMI's archive.
  while :; do
    st=$(ssm "systemctl is-active fcvm-fleet-vfio.service fcvm-fleet-store.service | paste -sd' '" | tr -d '\t' | head -1)
    case "$st" in "active active") break ;; *failed*) echo "host units: $st"; exit 1 ;; esac
    sleep 5
  done
  ssm "for u in fcvm-fleet-vfio fcvm-fleet-store; do echo \$u \$(date -d \"\$(systemctl show -p ActiveEnterTimestamp --value \$u.service)\" +%s%3N); done; journalctl -u fcvm-fleet-store -o cat --no-pager | grep 'fcvm-fleet-store:'" | while read -r u t rest; do
    case "$u" in fcvm-fleet-store:) echo "on-instance: $u $t $rest" ;; *) [ -z "$t" ] || echo "$u active $(( t - t0 )) ms after the request" ;; esac
  done
  # AWS-RunShellScript runs `sh` (dash on Ubuntu: no pipefail), so keep warm.sh's status
  # by hand and end with it. warm.log is fcvm's output: log lines and JSON records. The
  # restored record is printed whole (ack_ms, guest_phases). A snapshot miss also ends in
  # a restore, so hit or miss is read from fcvm's last snapshot-cache line: a hit whose
  # load fails falls back to a cold boot and logs "Snapshot miss" after the hit. Those
  # lines are info, and fcvm logs warnings and up unless RUST_LOG says otherwise.
  ssm "G0=\$(ls /sys/bus/pci/drivers/vfio-pci | grep -E '^[0-9a-f]{4}:' | head -1); prlimit --memlock=unlimited:unlimited sudo -u ubuntu -H XDG_RUNTIME_DIR=/run/user/1000 RUST_LOG=fcvm=info bash -c \"cd /tmp && bash /usr/local/lib/fcvm-fleet/warm.sh \$G0 /tmp/warm.log\" > /tmp/warm.out 2>&1; rc=\$?; tail -3 /tmp/warm.out; jq -cR 'fromjson? | objects | select(.status == \"restored\")' /tmp/warm.log | head -1; l=\$(grep -E 'snapshot hit|Snapshot miss' /tmp/warm.log | tail -1); case \"\$l\" in *'snapshot hit'*) echo 'snapshot: hit' ;; *'Snapshot miss'*) echo 'snapshot: miss' ;; *) echo 'snapshot: warm.log has no snapshot-cache line' ;; esac; echo \"\$l\"; exit \$rc" ||
    { echo "host: the first GPU VM failed"; exit 1; }
  t_vm=$(now)
  echo "host: first GPU VM done $((t_vm - t0)) ms after the request (includes SSM round trips)"
fi
