#!/bin/bash
# Keep boot-race-driver credentials fresh on the campaign box. Runs on the dev box, whose role may assume the driver
# role; the session lasts at most an hour (role chaining), so this writes a new one every 30 minutes to
# /dev/shm/boot-race-creds.json on the box, where race.py and prepare.py read it (race.py rereads it every round).
#   creds.sh <ssh command for the campaign box...>     e.g. creds.sh pbox ssh 3 --instance i-...
# Stop it with Ctrl-C or kill once the campaign's cleanup has run.
set -euo pipefail
[ $# -gt 0 ] || { echo "usage: creds.sh <ssh command for the campaign box>" >&2; exit 2; }
ROLE="arn:aws:iam::$(aws sts get-caller-identity --query Account --output text):role/boot-race-driver"
while true; do
  aws sts assume-role --role-arn "$ROLE" --role-session-name boot-race --duration-seconds 3600 \
      --query Credentials --output json |
    "$@" 'umask 077 && cat > /dev/shm/boot-race-creds.json.tmp && mv /dev/shm/boot-race-creds.json.tmp /dev/shm/boot-race-creds.json'
  echo "$(date -u +%H:%M:%S) credentials refreshed" >&2
  sleep 1800
done
