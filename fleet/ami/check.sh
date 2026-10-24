#!/bin/bash
# Offline checks of the fleet image recipes. CI does not run them (a path under fleet/
# outside fleet/core runs only the renderer's host tests); run before changing a recipe:
#   bash fleet/ami/check.sh
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 2
fail=0
no() { echo "FAIL: $*"; fail=1; }
scripts=(fleet/ami/*.sh fleet/ami/host/*.sh fleet/ami/tile/*.sh fleet/tools/gpubox.sh fleet/tools/issue-cert.sh)
for f in "${scripts[@]}"; do bash -n "$f" || no "bash -n $f"; done
if command -v shellcheck >/dev/null; then
  shellcheck -S warning "${scripts[@]}" || no "shellcheck -S warning"
else
  no "shellcheck is not installed"
fi
python3 -m py_compile fleet/ami/host/s3-cat.py fleet/ami/tile/fetch-cert.py || no "py_compile"

# `build-ami.sh start` must clear the phase's markers before it launches the phase:
# `wait` reads <phase>.done / <phase>.failed, and a builder from a BASE_AMI (or a
# repeated phase) still holds the previous run's, which `wait` would report at once.
start=$(sed -n '/^start)/,/;;/p' fleet/ami/build-ami.sh)
# A command of its own (`;`), so it has run when the SSM command returns; joined with `&&`
# it would be part of the backgrounded list and race the next `wait`.
clear_at=$(grep -bo 'rm -f /var/lib/fleet-ami/\$4.done /var/lib/fleet-ami/\$4.failed;' <<<"$start" | head -1 | cut -d: -f1)
launch_at=$(grep -bo 'nohup setsid' <<<"$start" | head -1 | cut -d: -f1)
[ -n "$clear_at" ] && [ -n "$launch_at" ] && [ "$clear_at" -lt "$launch_at" ] ||
  no "build-ami.sh start does not clear <phase>.done and <phase>.failed before launching the phase"

# The tile's image phase must fail when the renderer never serves a ready frame: the
# health endpoint answers 200 whatever its state, so the check is jq -e on its body.
ready='.ready == true and .frame > 0'
jq -e "$ready" <<<'{"ready":false,"frame":0,"gpu":{}}' >/dev/null && no "readiness predicate accepts a renderer that is not ready"
jq -e "$ready" <<<'{"ready":true,"frame":12,"gpu":{}}' >/dev/null || no "readiness predicate refuses a ready renderer"
image=$(sed -n '/^image)/,/;;/p' fleet/ami/tile/build.sh)
grep -qF "| jq -e '$ready' >/dev/null || { echo \"renderer not ready after 300 s\"" <<<"$image" ||
  no "tile image phase does not fail when the renderer is not ready after the wait"

# gpubox passes its lease script to `aws ec2 run-instances --user-data` as the script
# itself: the CLI base64-encodes user data, so a pre-encoded script reaches cloud-init as
# base64 text and the lease timer is never installed (seen 2026-10-24: describing a
# test instance's userData and decoding it once gave base64 text back).
grep -qE 'ud=\$\(lease_userdata "\$lease"\)$' fleet/tools/gpubox.sh ||
  no "gpubox.sh must pass the lease script to --user-data unencoded"

# measure-boot.sh must not report a timing for a command that failed on the instance: its
# ssm() fails unless the invocation ended Success. Run its ssm() against a stub `A` whose
# invocation ended Failed.
ssm_def=$(sed -n '/^ssm() {/,/^}/p' fleet/ami/measure-boot.sh)
if [ -z "$ssm_def" ]; then
  no "measure-boot.sh has no ssm() to test"
elif (
  A() { case "$*" in *send-command*) echo cmd-1 ;; *"--query Status"*) echo Failed ;; *) echo "warm: ok=0" ;; esac; }
  export id=i-1234567890abcdef0  # read by the eval'd ssm()
  eval "$ssm_def"
  ssm "true" >/dev/null 2>&1
); then
  no "measure-boot.sh ssm() returns success for an invocation that ended Failed"
fi
# The host warm-up command runs on the instance under `sh` (AWS-RunShellScript; dash on
# Ubuntu, which has no pipefail) and must end with warm.sh's status. Run the command as the
# instance receives it, under dash, with a stub prlimit standing in for the warm-up.
raw=$(sed -n 's/^  ssm "\(.*G0=.*\)" ||$/\1/p' fleet/ami/measure-boot.sh)
stub=$(mktemp -d)
printf '#!/bin/sh\necho "warm: ok=$WARM_OK"\n[ "$WARM_OK" = 1 ]\n' > "$stub/prlimit"; chmod +x "$stub/prlimit"
if [ -z "$raw" ] || ! command -v dash >/dev/null; then
  no "measure-boot.sh host warm-up command not found, or no dash to run it"
else
  remote=""; eval "remote=\"$raw\""
  WARM_OK=1 PATH="$stub:$PATH" dash -c "$remote" >/dev/null 2>&1 || no "measure-boot warm-up command fails under dash for a good warm-up"
  WARM_OK=0 PATH="$stub:$PATH" dash -c "$remote" >/dev/null 2>&1 && no "measure-boot warm-up command succeeds under dash for a failed warm-up"
fi
rm -rf "$stub"
grep -qF '{ echo "host: the first GPU VM failed"; exit 1; }' fleet/ami/measure-boot.sh ||
  no "measure-boot.sh host does not stop when the warm-up fails"

[ "$fail" = 0 ] && echo "fleet/ami checks: ok"
exit "$fail"
