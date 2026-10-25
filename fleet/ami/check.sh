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

# host/build.sh must check out $FCVM_COMMIT even when the builder's source tree (from a
# BASE_AMI) holds untracked files that commit tracks: on 2026-10-24 an untracked
# fleet/ami/ stopped the build with "untracked working tree files would be overwritten".
# Run build.sh's own checkout command in a scratch clone in that state.
checkout=$(sed -n 's/^  as_user "\(cd \$SRC && git fetch .*git checkout.*\)"$/\1/p' fleet/ami/host/build.sh)
gt=$(mktemp -d)
if [ -z "$checkout" ]; then
  no "host/build.sh has no source checkout command to test"
else
  (
    set -e
    g() { git -c user.email=check@example.invalid -c user.name=check -c init.defaultBranch=main "$@"; }
    g init -q "$gt/origin"; cd "$gt/origin"
    echo a > a; g add a; g commit -qm a; old=$(git rev-parse HEAD)
    mkdir -p fleet/ami; echo tracked > fleet/ami/warm.sh; g add fleet; g commit -qm b
    new=$(git rev-parse HEAD)
    g clone -q "$gt/origin" "$gt/src"; cd "$gt/src"; git checkout -q --detach "$old"
    mkdir -p fleet/ami; echo stray > fleet/ami/warm.sh; echo stray > leftover
    export SRC="$gt/src" FCVM_COMMIT="$new"  # read by the eval'd command
    c=""; eval "c=\"$checkout\""
    bash -c "$c" 2>&1 >/dev/null | sed "s/^/  checkout: /" >&2
    [ "$(git -C "$gt/src" rev-parse HEAD)" = "$new" ] && [ "$(cat "$gt/src/fleet/ami/warm.sh")" = tracked ] && [ ! -e "$gt/src/leftover" ]
  ) || no "host/build.sh's checkout does not reach FCVM_COMMIT cleanly over untracked files"
fi
rm -rf "$gt"

# The store unit writes the file the agent reads as --store-ready-file (the host's agent
# arguments, fleet/PROTOCOL.md "Running it"), with the boot times the agent reports. Nothing
# wrote it before, so every hello carried no store time.
agent_file=$(grep '^FLEET_AGENT_ARGS=' fleet/PROTOCOL.md | grep -o -- '--store-ready-file [^ ]*' | head -1 | cut -d' ' -f2)
store_file=$(sed -n 's/^READY=\${STORE_READY_FILE:-\([^}]*\)}$/\1/p' fleet/ami/host/store.sh)
[ -n "$agent_file" ] && [ "$agent_file" = "$store_file" ] ||
  no "store.sh writes '${store_file}', the agent reads --store-ready-file '${agent_file}'"
grep -q 'mv -f "$READY.tmp" "$READY"' fleet/ami/host/store.sh || no "store.sh does not replace the ready file by rename"
st=$(mktemp -d)
sed -n '/^printf .{"store_started_at"/,/> "\$READY.tmp"/p' fleet/ami/host/store.sh |
  sed "s|\$READY|$st/ready|; s|\$((t0 / 1000000))|1|; s|\$((t1 / 1000000))|2|; s|\"\$src\"|s3|" > "$st/w.sh"
bash "$st/w.sh" 2>/dev/null && jq -e '.store_started_at == 1 and .store_mounted_at == 2' "$st/ready.tmp" >/dev/null 2>&1 ||
  no "store.sh's ready file is not a JSON object of the store's times"
rm -rf "$st"

[ "$fail" = 0 ] && echo "fleet/ami checks: ok"
exit "$fail"
