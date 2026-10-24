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

[ "$fail" = 0 ] && echo "fleet/ami checks: ok"
exit "$fail"
